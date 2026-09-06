using System.Collections.ObjectModel;
using System.Diagnostics;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Threading;
using System.Threading.Channels;
using PersonalRag.FilenameSearch;

namespace PersonalRag.FilenameSearch.Gui;

public partial class MainWindow : Window
{
    private readonly ResultRows rows = [];
    private readonly string? rootOverride;
    private readonly string? storeOverride;
    private readonly string? startupProbePath;
    private readonly bool exitAfterProbe;
    private readonly string? startupProbeQuery;
    private readonly Channel<SearchWork> searchQueue = Channel.CreateUnbounded<SearchWork>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });
    private readonly Task searchWorker;
    private IFilenameCatalog? catalog;
    private CancellationTokenSource searchCancellation = new();
    private CancellationTokenSource liveRefreshCancellation = new();
    private long searchVersion;
    private int statusUpdateQueued;
    private int startupProbeWritten;
    private bool ready;
    private bool closing;

    public MainWindow(
        string? root = null,
        string? store = null,
        string? startupProbePath = null,
        bool exitAfterProbe = false,
        string? startupProbeQuery = null)
    {
        rootOverride = string.IsNullOrWhiteSpace(root) ? null : root;
        storeOverride = string.IsNullOrWhiteSpace(store) ? null : store;
        this.startupProbePath = startupProbePath;
        this.exitAfterProbe = exitAfterProbe;
        this.startupProbeQuery = startupProbeQuery;
        InitializeComponent();
        Results.ItemsSource = rows;
        searchWorker = Task.Factory.StartNew(SearchLoop, CancellationToken.None,
            TaskCreationOptions.DenyChildAttach | TaskCreationOptions.LongRunning, TaskScheduler.Default).Unwrap();
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        ready = false;
        try
        {
            SetStatus("ファイルシステムを読み込み中");
            if (rootOverride is not null)
            {
                string root = Path.GetFullPath(rootOverride);
                string store = storeOverride is null ? DefaultSingleStore(root) : Path.GetFullPath(storeOverride);
                catalog = await Task.Run(() => FileSystemCatalog.Open(root, store));
            }
            else
            {
                catalog = await MultiVolumeCatalog.OpenLocalFixedVolumesAsync(DefaultVolumeStoreRoot());
            }

            catalog.Changed += CatalogChanged;
            await catalog.Ready.ConfigureAwait(true);
            SetStatus($"{catalog.Status} · {catalog.RecordCount:N0} entries");
            if (startupProbeQuery is not null) FileQuery.Text = startupProbeQuery;
            ready = true;
            await SearchAsync();
            FileQuery.Focus();
        }
        catch (Exception ex)
        {
            SetStatus("開始エラー: " + ex.Message);
            Summary.Text = "検索を開始できませんでした。";
            WriteStartupProbeOnce(error: ex.ToString());
        }
    }

    private void CatalogChanged(CatalogChangeBatch batch)
    {
        if (closing) return;
        if (Interlocked.Exchange(ref statusUpdateQueued, 1) == 0)
        {
            Dispatcher.BeginInvoke(() =>
            {
                try
                {
                    if (closing || catalog is null) return;
                    SetStatus($"{catalog.Status} · {catalog.RecordCount:N0} entries · gen {catalog.Generation:N0}");
                }
                finally { Interlocked.Exchange(ref statusUpdateQueued, 0); }
            });
        }
        QueueLiveRefresh();
    }

    private void QueueLiveRefresh()
    {
        liveRefreshCancellation.Cancel();
        liveRefreshCancellation.Dispose();
        liveRefreshCancellation = new CancellationTokenSource();
        CancellationToken token = liveRefreshCancellation.Token;
        _ = Dispatcher.InvokeAsync(async () =>
        {
            try
            {
                await Task.Delay(100, token);
                if (!token.IsCancellationRequested && !closing) await SearchAsync();
            }
            catch (OperationCanceledException) { }
        });
    }

    private async void QueryChanged(object sender, RoutedEventArgs e)
    {
        if (ready) await SearchAsync();
    }

    private async Task SearchAsync()
    {
        searchCancellation.Cancel();
        searchCancellation.Dispose();
        searchCancellation = new CancellationTokenSource();
        CancellationToken token = searchCancellation.Token;
        long version = Interlocked.Increment(ref searchVersion);
        IFilenameCatalog? current = catalog;
        if (current is null) return;

        var request = new SearchRequest(
            FileQuery.Text,
            Scope.SelectedIndex == 1 ? SearchScope.FullPath : SearchScope.Filename,
            CaseSensitive.IsChecked == true,
            100,
            version);
        Summary.Text = "検索中…";
        try
        {
            var completion = new TaskCompletionSource<FilenameSearchResult>(
                TaskCreationOptions.RunContinuationsAsynchronously);
            searchQueue.Writer.TryWrite(new SearchWork(current, request, token, completion));
            FilenameSearchResult result = await completion.Task.WaitAsync(token);
            token.ThrowIfCancellationRequested();
            if (version != Volatile.Read(ref searchVersion) || closing) return;

            rows.ReplaceAll(result.Records.Select(record => new ResultRow(record)));
            Summary.Text = $"{rows.Count:N0}件表示 · {result.ElapsedMs:F1} ms";
            SetStatus($"{current.Status} · {current.RecordCount:N0} entries · gen {current.Generation:N0}");
            WriteStartupProbeOnce();
        }
        catch (OperationCanceledException) { }
        catch (Exception ex)
        {
            if (version == Volatile.Read(ref searchVersion))
                Summary.Text = "検索エラー: " + ex.Message;
            WriteStartupProbeOnce(error: ex.ToString());
        }
    }

    private void WriteStartupProbeOnce(string? error = null)
    {
        if (startupProbePath is null) return;
        if (Interlocked.Exchange(ref startupProbeWritten, 1) != 0) return;
        try
        {
            using var process = Process.GetCurrentProcess();
            process.Refresh();
            string full = Path.GetFullPath(startupProbePath);
            Directory.CreateDirectory(Path.GetDirectoryName(full)!);
            var payload = new
            {
                ready = error is null,
                error,
                rows = rows.Count,
                private_bytes = process.PrivateMemorySize64,
                generation = catalog?.Generation ?? 0,
                entries = catalog?.RecordCount ?? 0,
                status = catalog?.Status ?? Status.Text,
                utc = DateTime.UtcNow
            };
            File.WriteAllText(full, JsonSerializer.Serialize(payload, new JsonSerializerOptions { WriteIndented = true }));
        }
        finally
        {
            if (exitAfterProbe)
                _ = Dispatcher.BeginInvoke(DispatcherPriority.ApplicationIdle, new Action(Close));
        }
    }

    private void ResultKey(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter)
        {
            e.Handled = true;
            OpenFile();
        }
    }

    private void SelectionChanged(object sender, SelectionChangedEventArgs e) { }
    private void OpenSelected(object sender, MouseButtonEventArgs e) => OpenFile();

    private void OpenFile()
    {
        if (Results.SelectedItem is not ResultRow row) return;
        try { Process.Start(new ProcessStartInfo(row.Record.FullPath) { UseShellExecute = true }); }
        catch (Exception ex) { SetStatus("開けません: " + ex.Message); }
    }

    private void OnGlobalKey(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.F && Keyboard.Modifiers.HasFlag(ModifierKeys.Control))
        {
            FileQuery.Focus();
            FileQuery.SelectAll();
            e.Handled = true;
        }
        else if (e.Key == Key.Escape && Keyboard.FocusedElement is TextBox box && box.IsEnabled)
        {
            box.Clear();
            e.Handled = true;
        }
    }

    private async void OnClosing(object? sender, System.ComponentModel.CancelEventArgs e)
    {
        if (closing) return;
        e.Cancel = true;
        closing = true;
        searchCancellation.Cancel();
        liveRefreshCancellation.Cancel();
        searchQueue.Writer.TryComplete();
        try { await searchWorker; } catch (OperationCanceledException) { }
        if (catalog is not null)
        {
            catalog.Changed -= CatalogChanged;
            await catalog.DisposeAsync();
        }
        searchCancellation.Dispose();
        liveRefreshCancellation.Dispose();
        _ = Dispatcher.BeginInvoke(DispatcherPriority.ApplicationIdle, new Action(Close));
    }

    private void SetStatus(string value) => Status.Text = value;

    private async Task SearchLoop()
    {
        await foreach (SearchWork work in searchQueue.Reader.ReadAllAsync())
        {
            if (work.Token.IsCancellationRequested)
            {
                work.Completion.TrySetCanceled(work.Token);
                continue;
            }
            try { work.Completion.TrySetResult(work.Catalog.Search(work.Request)); }
            catch (Exception ex) { work.Completion.TrySetException(ex); }
        }
    }

    private static string DefaultSingleStore(string root)
    {
        string appData = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        string key = Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(
            System.Text.Encoding.UTF8.GetBytes(root))).ToLowerInvariant()[..16];
        return Path.Combine(appData, "PersonalRag", "filename-index", "single", key + ".manifest");
    }

    private static string DefaultVolumeStoreRoot()
    {
        string appData = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        return Path.Combine(appData, "PersonalRag", "filename-index", "volumes");
    }

    private sealed record SearchWork(
        IFilenameCatalog Catalog,
        SearchRequest Request,
        CancellationToken Token,
        TaskCompletionSource<FilenameSearchResult> Completion);
}

public sealed class ResultRow
{
    public ResultRow(FilenameRecord record) => Record = record;
    public FilenameRecord Record { get; }
    public string Name => Record.Name;
    public string FullPath => Record.FullPath;
    public string Size => Record.IsDirectory ? "<DIR>" : $"{Record.SizeBytes:N0} B";
    public string Modified => Record.ModifiedUtc.ToLocalTime().ToString("yyyy/MM/dd HH:mm");
}

internal sealed class ResultRows : ObservableCollection<ResultRow>
{
    public void ReplaceAll(IEnumerable<ResultRow> values)
    {
        Items.Clear();
        foreach (ResultRow value in values) Items.Add(value);
        OnPropertyChanged(new System.ComponentModel.PropertyChangedEventArgs(nameof(Count)));
        OnPropertyChanged(new System.ComponentModel.PropertyChangedEventArgs("Item[]"));
        OnCollectionChanged(new System.Collections.Specialized.NotifyCollectionChangedEventArgs(
            System.Collections.Specialized.NotifyCollectionChangedAction.Reset));
    }
}

using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Threading;
using System.Threading.Channels;
using PersonalRag.FilenameSearch;
using Microsoft.Win32;

namespace PersonalRag.FilenameSearch.Gui;

public partial class MainWindow : Window
{
    private readonly ResultRows rows = [];
    private readonly string? rootOverride;
    private readonly string? storeOverride;
    private readonly Channel<SearchWork> searchQueue = Channel.CreateUnbounded<SearchWork>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });
    private readonly Task searchWorker;
    private FileSystemCatalog? catalog;
    private CancellationTokenSource searchCancellation = new();
    private long searchVersion;
    private int statusUpdateQueued;
    private bool ready;
    private bool closing;

    public MainWindow(string? root = null, string? store = null)
    {
        rootOverride = root;
        storeOverride = store;
        InitializeComponent();
        Results.ItemsSource = rows;
        searchWorker = Task.Factory.StartNew(SearchLoop, CancellationToken.None,
            TaskCreationOptions.DenyChildAttach | TaskCreationOptions.LongRunning, TaskScheduler.Default).Unwrap();
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        ready = true;
        string? root = rootOverride;
        if (string.IsNullOrWhiteSpace(root))
        {
            var dialog = new OpenFolderDialog { Title = "検索対象フォルダーを選択してください" };
            if (dialog.ShowDialog(this) != true)
            {
                SetStatus("検索対象が未設定です");
                Summary.Text = "検索対象が未設定です。";
                return;
            }
            root = dialog.FolderName;
        }
        try
        {
            root = Path.GetFullPath(root);
            string store = storeOverride is null ? DefaultStore(root) : Path.GetFullPath(storeOverride);
            SetStatus("ファイルシステムを読み込み中");
            catalog = await Task.Run(() => FileSystemCatalog.Open(root, store));
            catalog.Changed += CatalogChanged;
            await catalog.Ready.ConfigureAwait(true);
            SetStatus(catalog.Status);
            await SearchAsync();
            FileQuery.Focus();
        }
        catch (Exception ex)
        {
            SetStatus("開始エラー: " + ex.Message);
            Summary.Text = "検索を開始できませんでした。";
        }
    }

    private void CatalogChanged()
    {
        if (closing) return;
        if (Interlocked.Exchange(ref statusUpdateQueued, 1) != 0) return;
        Dispatcher.BeginInvoke(() =>
        {
            try
            {
                if (closing || catalog is null) return;
                SetStatus($"{catalog.Status} · {catalog.Records.Count:N0} entries");
            }
            finally { Interlocked.Exchange(ref statusUpdateQueued, 0); }
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
        FileSystemCatalog? current = catalog;
        if (current is null) return;
        var request = new SearchRequest(FileQuery.Text,
            Scope.SelectedIndex == 1 ? SearchScope.FullPath : SearchScope.Filename,
            CaseSensitive.IsChecked == true, 25, version);
        Summary.Text = "検索中…";
        try
        {
            var completion = new TaskCompletionSource<FilenameSearchResult>(TaskCreationOptions.RunContinuationsAsynchronously);
            searchQueue.Writer.TryWrite(new SearchWork(current, request, token, completion));
            FilenameSearchResult result = await completion.Task.WaitAsync(token);
            token.ThrowIfCancellationRequested();
            if (version != Volatile.Read(ref searchVersion) || closing) return;
            rows.ReplaceAll(result.Records.Select(record => new ResultRow(record)));
            Summary.Text = $"{rows.Count:N0}件表示 · {result.ElapsedMs:F1} ms";
            SetStatus($"{current.Status} · {current.Records.Count:N0} entries");
        }
        catch (OperationCanceledException) { }
        catch (Exception ex)
        {
            if (version == Volatile.Read(ref searchVersion)) Summary.Text = "検索エラー: " + ex.Message;
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
        searchQueue.Writer.TryComplete();
        try { await searchWorker; } catch (OperationCanceledException) { }
        if (catalog is not null) await catalog.DisposeAsync();
        // Closing is a synchronous WPF event. Schedule the second Close after the
        // current close callback has returned; the `closing` guard lets that call pass.
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

    private static string DefaultStore(string root)
    {
        string appData = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        string key = Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(System.Text.Encoding.UTF8.GetBytes(root))).ToLowerInvariant()[..16];
        return Path.Combine(appData, "PersonalRagAstra", "filename-index", key + ".routec");
    }

    private sealed record SearchWork(FileSystemCatalog Catalog, SearchRequest Request, CancellationToken Token,
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

/// <summary>Publishes one reset notification for a result batch instead of one per row.</summary>
internal sealed class ResultRows : ObservableCollection<ResultRow>
{
    public void ReplaceAll(IEnumerable<ResultRow> values)
    {
        Items.Clear();
        foreach (ResultRow value in values) Items.Add(value);
        OnPropertyChanged(new System.ComponentModel.PropertyChangedEventArgs(nameof(Count)));
        OnPropertyChanged(new System.ComponentModel.PropertyChangedEventArgs("Item[]"));
        OnCollectionChanged(new System.Collections.Specialized.NotifyCollectionChangedEventArgs(System.Collections.Specialized.NotifyCollectionChangedAction.Reset));
    }
}

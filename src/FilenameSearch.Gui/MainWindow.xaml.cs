using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
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
    private readonly string? startupProbeMode;
    private readonly Task<IFilenameCatalog> catalogOpenTask;
    private readonly Channel<SearchWork> searchQueue = Channel.CreateUnbounded<SearchWork>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });
    private readonly Task searchWorker;
    private IFilenameCatalog? catalog;
    private CancellationTokenSource searchCancellation = new();
    private CancellationTokenSource liveRefreshCancellation = new();
    private long searchVersion;
    private int statusUpdateQueued;
    private int startupProbeWritten;
    private bool probeDrivingQuery;
    private bool ready;
    private bool closing;
    private double lastCoreSearchElapsedMs;
    private double lastRowsReplaceElapsedMs;

    public MainWindow(
        string? root = null,
        string? store = null,
        string? startupProbePath = null,
        bool exitAfterProbe = false,
        string? startupProbeQuery = null,
        string? startupProbeMode = null)
    {
        rootOverride = string.IsNullOrWhiteSpace(root) ? null : root;
        storeOverride = string.IsNullOrWhiteSpace(store) ? null : store;
        this.startupProbePath = startupProbePath;
        this.exitAfterProbe = exitAfterProbe;
        this.startupProbeQuery = startupProbeQuery;
        this.startupProbeMode = startupProbeMode ?? (startupProbePath is null ? null : "startup");
        catalogOpenTask = OpenCatalogAsync();
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
            catalog = await catalogOpenTask.ConfigureAwait(true);
            if (catalog is FileSystemCatalog startupCatalog)
                startupCatalog.DeferBasePathIndexBuild();

            catalog.Changed += CatalogChanged;
            await catalog.Ready.ConfigureAwait(true);
            SetStatus($"{catalog.Status} · {catalog.RecordCount:N0} entries");
            if (startupProbeQuery is not null) FileQuery.Text = startupProbeQuery;
            ready = true;
            await SearchAsync();
            if (catalog is FileSystemCatalog settledCatalog)
                settledCatalog.StartBasePathIndexBuild();
            FileQuery.Focus();
        }
        catch (Exception ex)
        {
            if (catalog is FileSystemCatalog failedCatalog)
                failedCatalog.StartBasePathIndexBuild();
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
        if (ready && !probeDrivingQuery) await SearchAsync();
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

            Stopwatch rowsWatch = Stopwatch.StartNew();
            rows.ReplaceAll(result.Records.Select(record => new ResultRow(record)));
            rowsWatch.Stop();
            Volatile.Write(ref lastRowsReplaceElapsedMs, rowsWatch.Elapsed.TotalMilliseconds);
            Volatile.Write(ref lastCoreSearchElapsedMs, result.ElapsedMs);
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
        if (error is null && startupProbeMode is "warm" or "live")
        {
            _ = startupProbeMode == "warm" ? RunWarmProbeAsync() : RunLiveProbeAsync();
            return;
        }
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

    private Task<IFilenameCatalog> OpenCatalogAsync()
    {
        if (rootOverride is not null)
        {
            string root = Path.GetFullPath(rootOverride);
            string store = storeOverride is null ? DefaultSingleStore(root) : Path.GetFullPath(storeOverride);
            return Task.Factory.StartNew<IFilenameCatalog>(
                () => FileSystemCatalog.OpenDeferred(root, store),
                CancellationToken.None,
                TaskCreationOptions.DenyChildAttach | TaskCreationOptions.LongRunning,
                TaskScheduler.Default);
        }
        return MultiVolumeCatalog.OpenLocalFixedVolumesAsync(DefaultVolumeStoreRoot())
            .ContinueWith<IFilenameCatalog>(task => task.GetAwaiter().GetResult(),
                CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }

    private async Task RunWarmProbeAsync()
    {
        try
        {
            // Initial store opening may still be replaying a persisted watcher gap after
            // the first useful batch is shown.  Warm-input samples describe steady user
            // interaction, so drain that already-visible catch-up before starting the
            // fixed 20-round sequence; the separate startup probe measures the first batch.
            if (catalog is FileSystemCatalog one)
                await one.WaitForIdleAsync(TimeSpan.FromMinutes(5));
            GuiProbeQuery[] queries =
            [
                new("fixture_", SearchScope.Filename),
                new("Ω", SearchScope.Filename),
                new("zz", SearchScope.Filename),
                new("report_*.xlsx", SearchScope.Filename),
                new("X?Z", SearchScope.Filename),
                new("fixture_0000001", SearchScope.FullPath),
                new("日本語", SearchScope.Filename),
                new("absent_formal_query", SearchScope.Filename)
            ];
            Random random = new(123456);
            GuiProbeQuery[] order = queries.OrderBy(_ => random.Next()).ToArray();
            var warmup = new List<double>();
            var measured = new List<double>();
            var coreMeasured = new List<double>();
            var rowsMeasured = new List<double>();
            var gcMeasured = new List<int[]>();
            int gc0Before = GC.CollectionCount(0), gc1Before = GC.CollectionCount(1), gc2Before = GC.CollectionCount(2);
            for (int round = 0; round < 20; round++)
            {
                foreach (GuiProbeQuery query in order)
                {
                    Stopwatch watch = Stopwatch.StartNew();
                    probeDrivingQuery = true;
                    try
                    {
                        Scope.SelectedIndex = query.Scope == SearchScope.FullPath ? 1 : 0;
                        FileQuery.Text = query.Query;
                        await SearchAsync();
                    }
                    finally { probeDrivingQuery = false; }
                    watch.Stop();
                    (round < 2 ? warmup : measured).Add(watch.Elapsed.TotalMilliseconds);
                    if (round >= 2) coreMeasured.Add(Volatile.Read(ref lastCoreSearchElapsedMs));
                    if (round >= 2)
                    {
                        rowsMeasured.Add(Volatile.Read(ref lastRowsReplaceElapsedMs));
                        gcMeasured.Add([GC.CollectionCount(0) - gc0Before, GC.CollectionCount(1) - gc1Before, GC.CollectionCount(2) - gc2Before]);
                    }
                }
            }
            using Process process = Process.GetCurrentProcess();
            process.Refresh();
            double p95 = Percentile(measured, .95), p99 = Percentile(measured, .99), max = measured.Count == 0 ? 0 : measured.Max();
            WriteProbeJson(new
            {
                version = 1, mode = "warm-input", source_commit = SourceCommit(), benchmark_rounds = 20,
                warmup_rounds = 2, measured_rounds = 18, query_shuffle_seed = 123456,
                warmup_samples_ms = warmup, measured_samples_ms = measured, core_search_samples_ms = coreMeasured,
                rows_replace_samples_ms = rowsMeasured, gc_deltas = gcMeasured,
                p50_ms = Percentile(measured, .50), p95_ms = p95, p99_ms = p99, max_ms = max,
                private_bytes = process.PrivateMemorySize64, entries = catalog?.RecordCount ?? 0,
                pass = p95 <= 100 && max <= 200, utc = DateTime.UtcNow
            });
        }
        catch (Exception ex) { WriteProbeJson(new { version = 1, mode = "warm-input", source_commit = SourceCommit(), pass = false, error = ex.ToString(), utc = DateTime.UtcNow }); }
        finally { CloseAfterProbe(); }
    }

    private async Task RunLiveProbeAsync()
    {
        string? fixture = null;
        string? renamed = null;
        string? directory = null;
        var changeTrace = new List<object>();
        object traceGate = new();
        DateTime traceStarted = DateTime.UtcNow;
        void Trace(CatalogChangeBatch batch)
        {
            lock (traceGate)
                changeTrace.Add(new { ms = (DateTime.UtcNow - traceStarted).TotalMilliseconds,
                    generation = batch.Generation, reconciled = batch.Reconciled,
                    changes = batch.Changes.Select(change => new { kind = change.Kind.ToString(), path = change.After?.FullPath ?? change.Before?.FullPath }).ToArray() });
        }
        try
        {
            string root = catalog is FileSystemCatalog one ? one.Root : rootOverride ?? throw new InvalidOperationException("live probe requires a single root catalog");
            // Keep startup catch-up separate from the live-refresh sample.  A newly-opened
            // store may still be reconciling a watcher gap after the first useful batch;
            // user edits are measured once that initial catalog work has settled.
            if (catalog is FileSystemCatalog settled)
                await settled.WaitForIdleAsync(TimeSpan.FromMinutes(5));
            catalog!.Changed += Trace;
            traceStarted = DateTime.UtcNow;
            directory = Path.Combine(root, ".formal-gui-live-" + Environment.ProcessId);
            Directory.CreateDirectory(directory);
            string stem = "gui_live_probe_" + Environment.ProcessId;
            fixture = Path.Combine(directory, stem + ".txt");
            renamed = Path.Combine(directory, stem + "_renamed.txt");
            probeDrivingQuery = true;
            Scope.SelectedIndex = 0;
            FileQuery.Text = stem;
            await SearchAsync();
            probeDrivingQuery = false;
            if (rows.Count != 0) throw new InvalidOperationException("live fixture query was not initially empty");
            Stopwatch createWatch = Stopwatch.StartNew();
            File.WriteAllText(fixture, "live");
            await UntilUiAsync(() => rows.Any(row => row.FullPath.Equals(fixture, StringComparison.Ordinal)), "GUI live create");
            createWatch.Stop();
            Stopwatch renameWatch = Stopwatch.StartNew();
            File.Move(fixture, renamed);
            await UntilUiAsync(() => rows.Any(row => row.FullPath.Equals(renamed, StringComparison.Ordinal)), "GUI live rename");
            renameWatch.Stop();
            Stopwatch deleteWatch = Stopwatch.StartNew();
            File.Delete(renamed);
            await UntilUiAsync(() => rows.Count == 0, "GUI live delete");
            deleteWatch.Stop();
            lock (traceGate)
                WriteProbeJson(new { version = 1, mode = "live-refresh", source_commit = SourceCommit(), create_ms = createWatch.Elapsed.TotalMilliseconds, rename_ms = renameWatch.Elapsed.TotalMilliseconds, delete_ms = deleteWatch.Elapsed.TotalMilliseconds, pass = createWatch.Elapsed.TotalMilliseconds <= 1000 && renameWatch.Elapsed.TotalMilliseconds <= 1000 && deleteWatch.Elapsed.TotalMilliseconds <= 1000, change_trace = changeTrace.ToArray(), utc = DateTime.UtcNow });
        }
        catch (Exception ex) { lock (traceGate) WriteProbeJson(new { version = 1, mode = "live-refresh", source_commit = SourceCommit(), pass = false, error = ex.ToString(), change_trace = changeTrace.ToArray(), utc = DateTime.UtcNow }); }
        finally
        {
            if (catalog is not null) catalog.Changed -= Trace;
            probeDrivingQuery = false;
            if (fixture is not null) { try { if (File.Exists(fixture)) File.Delete(fixture); } catch { } }
            if (renamed is not null) { try { if (File.Exists(renamed)) File.Delete(renamed); } catch { } }
            if (directory is not null) { try { if (Directory.Exists(directory) && !new DirectoryInfo(directory).EnumerateFileSystemInfos().Any()) Directory.Delete(directory); } catch { } }
            CloseAfterProbe();
        }
    }

    private void WriteProbeJson(object payload)
    {
        if (startupProbePath is null) return;
        string full = Path.GetFullPath(startupProbePath);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        File.WriteAllText(full, JsonSerializer.Serialize(payload, new JsonSerializerOptions { WriteIndented = true }));
    }

    private void CloseAfterProbe()
    {
        if (exitAfterProbe) _ = Dispatcher.BeginInvoke(DispatcherPriority.ApplicationIdle, new Action(Close));
    }

    private async Task UntilUiAsync(Func<bool> condition, string name)
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        while (!condition())
        {
            timeout.Token.ThrowIfCancellationRequested();
            await Task.Delay(10, timeout.Token);
        }
    }

    private static double Percentile(IReadOnlyList<double> values, double percentile)
    {
        if (values.Count == 0) return 0;
        double[] sorted = values.OrderBy(value => value).ToArray();
        return sorted[Math.Clamp((int)Math.Ceiling(sorted.Length * percentile) - 1, 0, sorted.Length - 1)];
    }

    private static string SourceCommit()
    {
        try
        {
            using Process process = Process.Start(new ProcessStartInfo("git", "rev-parse HEAD") { UseShellExecute = false, RedirectStandardOutput = true, CreateNoWindow = true })!;
            string value = process.StandardOutput.ReadToEnd().Trim(); process.WaitForExit(5000); return value;
        }
        catch { return "unknown"; }
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

    private sealed record GuiProbeQuery(string Query, SearchScope Scope);
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

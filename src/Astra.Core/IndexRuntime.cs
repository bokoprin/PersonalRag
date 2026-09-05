using System.Threading.Channels;

namespace Astra.Core;

public sealed class IndexRuntime : IAsyncDisposable
{
    private readonly string store;
    private readonly FileSystemWatcher watcher;
    private readonly CancellationTokenSource stop = new();
    private readonly Channel<string> changes = Channel.CreateBounded<string>(new BoundedChannelOptions(100000) { FullMode = BoundedChannelFullMode.Wait });
    private readonly Channel<IndexSnapshot> saves = Channel.CreateBounded<IndexSnapshot>(new BoundedChannelOptions(1) { FullMode = BoundedChannelFullMode.DropOldest });
    private readonly Task worker, saver;
    private readonly IndexWriteLease lease;
    private IndexSnapshot snapshot;
    private IndexSnapshot? persisted;
    private int rescan;
    public IndexSnapshot Snapshot => Volatile.Read(ref snapshot);
    public bool IsSettled => Status == "Ready" && ReferenceEquals(Snapshot, Volatile.Read(ref persisted)) && changes.Reader.Count == 0;
    public string Status { get; private set; } = "差分を確認中";
    public event Action? Changed;

    public IndexRuntime(string store, IndexSnapshot snapshot)
    {
        this.store = Path.GetFullPath(store); this.snapshot = snapshot;
        watcher = new FileSystemWatcher(snapshot.Root) { IncludeSubdirectories = true, InternalBufferSize = 65536,
            NotifyFilter = NotifyFilters.FileName | NotifyFilters.DirectoryName | NotifyFilters.LastWrite | NotifyFilters.Size };
        try { lease = IndexStore.AcquireWriter(this.store); }
        catch { watcher.Dispose(); throw; }
        watcher.Created += (_, e) => Queue(e.FullPath);
        watcher.Changed += (_, e) => Queue(e.FullPath);
        watcher.Deleted += (_, e) => Queue(e.FullPath);
        watcher.Renamed += (_, e) =>
        {
            if (Directory.Exists(e.FullPath)) Interlocked.Exchange(ref rescan, 1);
            Queue(e.OldFullPath); Queue(e.FullPath);
        };
        watcher.Error += (_, _) => { Interlocked.Exchange(ref rescan, 1); Queue(snapshot.Root); };
        try { watcher.EnableRaisingEvents = true; }
        catch { watcher.Dispose(); lease.Dispose(); throw; }
        Interlocked.Exchange(ref rescan, 1); Queue(snapshot.Root);
        worker = Task.Run(UpdateLoop); saver = Task.Run(SaveLoop);
    }
    private void Queue(string path)
    {
        if (!changes.Writer.TryWrite(path)) Interlocked.Exchange(ref rescan, 1);
    }
    private async Task UpdateLoop()
    {
        var builder = new IndexBuilder();
        try
        {
            while (await changes.Reader.WaitToReadAsync(stop.Token))
            {
                await Task.Delay(30, stop.Token);
                var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
                while (changes.Reader.TryRead(out var path)) paths.Add(path);
                try
                {
                    Status = "変更を反映中"; Changed?.Invoke();
                    IndexSnapshot next;
                    if (Interlocked.Exchange(ref rescan, 0) != 0 || paths.Any(Directory.Exists))
                        next = builder.Build(Snapshot.Root, stop.Token, previous: Snapshot);
                    else
                    {
                        var prior = Snapshot;
                        var replacements = new List<FileEntry>();
                        var deletedDirectories = new List<string>();
                        foreach (string path in paths)
                        {
                            stop.Token.ThrowIfCancellationRequested();
                            if (File.Exists(path)) replacements.Add(builder.ReadEntry(path, stop.Token));
                            else
                            {
                                int found = Array.BinarySearch(prior.Files, new FileEntry(path, 0, 0, null, ReadOnlyMemory<byte>.Empty), EntryPathComparer.Instance);
                                if (found < 0) deletedDirectories.Add(Path.TrimEndingDirectorySeparator(path) + Path.DirectorySeparatorChar);
                            }
                        }
                        replacements.Sort(EntryPathComparer.Instance);
                        var merged = new List<FileEntry>(prior.Files.Length + replacements.Count);
                        int inserted = 0;
                        foreach (var entry in prior.Files)
                        {
                            if (paths.Contains(entry.Path) || deletedDirectories.Any(prefix => entry.Path.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))) continue;
                            while (inserted < replacements.Count && EntryPathComparer.Instance.Compare(replacements[inserted], entry) < 0)
                                merged.Add(replacements[inserted++]);
                            merged.Add(entry);
                        }
                        while (inserted < replacements.Count) merged.Add(replacements[inserted++]);
                        next = new IndexSnapshot(prior.Root, merged.ToArray(), DateTime.UtcNow);
                    }
                    Volatile.Write(ref snapshot, next); saves.Writer.TryWrite(next);
                    Status = "Ready"; Changed?.Invoke();
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or AggregateException)
                {
                    Status = "変更反映エラー: " + ex.GetBaseException().Message; Changed?.Invoke();
                    // A transient rename/write race must not permanently lose the corresponding event.
                    Interlocked.Exchange(ref rescan, 1);
                    await Task.Delay(500, stop.Token); Queue(Snapshot.Root);
                }
            }
        }
        catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
        finally { saves.Writer.TryComplete(); }
    }
    private async Task SaveLoop()
    {
        await foreach (var pending in saves.Reader.ReadAllAsync())
        {
            var newest = pending;
            await Task.Delay(100);
            while (saves.Reader.TryRead(out var later)) newest = later;
            try { IndexStore.Save(store, newest, lease); Volatile.Write(ref persisted, newest); }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                Status = "保存エラー: " + ex.Message; Changed?.Invoke();
                if (!stop.IsCancellationRequested)
                { await Task.Delay(500); saves.Writer.TryWrite(Snapshot); }
            }
        }
    }
    public async ValueTask DisposeAsync()
    {
        watcher.EnableRaisingEvents = false; watcher.Dispose(); stop.Cancel(); changes.Writer.TryComplete();
        try { await worker; await saver; }
        finally { lease.Dispose(); stop.Dispose(); }
    }
}

internal sealed class EntryPathComparer : IComparer<FileEntry>
{
    public static readonly EntryPathComparer Instance = new();
    public int Compare(FileEntry? x, FileEntry? y) => StringComparer.OrdinalIgnoreCase.Compare(x?.Path, y?.Path);
}

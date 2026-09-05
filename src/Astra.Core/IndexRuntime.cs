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
    private int rescan;
    public IndexSnapshot Snapshot => Volatile.Read(ref snapshot);
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
        watcher.Renamed += (_, e) => { Queue(e.OldFullPath); Queue(e.FullPath); Interlocked.Exchange(ref rescan, 1); };
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
                        var map = Snapshot.Files.ToDictionary(f => f.Path, StringComparer.OrdinalIgnoreCase);
                        foreach (string path in paths)
                        {
                            stop.Token.ThrowIfCancellationRequested();
                            if (File.Exists(path)) map[path] = builder.ReadEntry(path, stop.Token);
                            else
                            {
                                map.Remove(path);
                                string prefix = Path.TrimEndingDirectorySeparator(path) + Path.DirectorySeparatorChar;
                                foreach (var descendant in map.Keys.Where(p => p.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)).ToArray()) map.Remove(descendant);
                            }
                        }
                        next = new IndexSnapshot(Snapshot.Root, map.Values.OrderBy(f => f.Path, StringComparer.OrdinalIgnoreCase).ToArray(), DateTime.UtcNow);
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
            try { IndexStore.Save(store, newest, lease); }
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

using System.Collections.Concurrent;
using System.Threading.Channels;

namespace PersonalRag.FilenameSearch;

internal enum FileSystemEventKind { Created, Changed, Deleted, Renamed }
internal sealed record FileSystemEvent(
    string Path, string? OldPath = null, FileSystemEventKind Kind = FileSystemEventKind.Changed,
    bool Reconcile = false, bool CatchUp = false);

/// <summary>One-volume exact-metadata catalog with bounded change ingestion and durable generation deltas.</summary>
public sealed class FileSystemCatalog : IFilenameCatalog
{
    private readonly object gate = new();
    private readonly ReaderWriterLockSlim engineGate = new();
    private readonly string root, store, rootIdentity, volumeId;
    private readonly string[] excludedRoots;
    private readonly GenerationStore persistence;
    private readonly IVolumeChangeFeed changeFeed;
    private readonly Channel<FileSystemEvent> events = Channel.CreateBounded<FileSystemEvent>(
        new BoundedChannelOptions(8192) { SingleReader = true, SingleWriter = false, FullMode = BoundedChannelFullMode.Wait });
    private readonly CancellationTokenSource stop = new();
    private readonly Task worker;
    private readonly TaskCompletionSource ready = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly Dictionary<string, FilenameRecord> byPath = new(StringComparer.Ordinal);
    private FilenameSearchEngine engine;
    private int nextId = 1, pendingEvents, maxPendingEvents, forceReconcile, compactionRunning;
    private long queueSaturationCount, reconcileCount, compactionCount;
    private long generation;
    private string status = "準備中";
    private bool started;
    private volatile bool disposed;

    private FileSystemCatalog(string root, string store, string rootIdentity, string volumeId,
        GenerationStore persistence, FilenameSearchEngine engine, IEnumerable<string>? excludedRoots)
    {
        this.root = Path.GetFullPath(root);
        this.store = Path.GetFullPath(store);
        this.rootIdentity = rootIdentity;
        this.volumeId = volumeId;
        this.persistence = persistence;
        this.engine = engine;
        this.excludedRoots = (excludedRoots ?? []).Select(Path.GetFullPath)
            .Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
        changeFeed = VolumeChangeFeedFactory.Create(this.root);
        changeFeed.Changed += Queue;
        changeFeed.Overflow += () => { Interlocked.Exchange(ref forceReconcile, 1); SignalReconcile(); };
        worker = Task.Run(UpdateLoop);
    }

    public string Root => root;
    public string Store => store;
    public string RootIdentity => rootIdentity;
    public string VolumeId => volumeId;
    public string Status => Volatile.Read(ref status);
    public long Generation => Interlocked.Read(ref generation);
    public int RecordCount { get { lock (gate) return byPath.Count; } }
    public bool IsReady => ready.Task.IsCompletedSuccessfully;
    public Task Ready => ready.Task;
    public bool WasDirtyShutdown => persistence.WasDirtyShutdown;
    public event Action<CatalogChangeBatch>? Changed;
    public IReadOnlyList<FilenameRecord> Records { get { lock (gate) return byPath.Values.OrderBy(r => r.FileId).ToArray(); } }

    internal FilenameCatalogDiagnostics GetDiagnostics() => new(
        Volatile.Read(ref pendingEvents),
        Volatile.Read(ref maxPendingEvents),
        Interlocked.Read(ref queueSaturationCount),
        Interlocked.Read(ref reconcileCount),
        Interlocked.Read(ref compactionCount),
        persistence.FullBaseRewriteCount,
        persistence.DeltaChangeCount,
        persistence.DeltaBytes,
        persistence.PersistenceWriteBytes,
        Generation,
        persistence.BaseGeneration);

    internal bool ContainsPath(string path)
    {
        string full = Path.GetFullPath(path);
        lock (gate) return byPath.ContainsKey(full);
    }

    public static FileSystemCatalog Open(string root, string store, IEnumerable<string>? excludedRoots = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(root);
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string fullRoot = Path.GetFullPath(root);
        if (!Directory.Exists(fullRoot)) throw new DirectoryNotFoundException(fullRoot);
        string volumeId = VolumeIdentity.GetVolumeId(fullRoot);
        FileKey rootKey = VolumeIdentity.GetFileKey(fullRoot, true, volumeId);
        string identity = "root|" + rootKey;
        var persistence = new GenerationStore(identity, store);
        var engine = new FilenameSearchEngine();
        var catalog = new FileSystemCatalog(fullRoot, store, identity, volumeId, persistence, engine, excludedRoots);
        try
        {
            bool loaded = false;
            try
            {
                loaded = persistence.TryLoad(engine, out long persisted);
                if (loaded) { catalog.generation = persisted; catalog.LoadExistingRecords(); }
            }
            catch (Exception ex) when (ex is IOException or InvalidDataException or UnauthorizedAccessException or System.Text.Json.JsonException)
            {
                catalog.SetStatus("保存済みインデックスを破棄して再構築します: " + ex.Message);
                persistence.ResetCorruptStore();
                engine.Build([]);
            }
            if (!loaded) catalog.RebuildInitial();
            catalog.SetStatus(loaded && persistence.WasDirtyShutdown ? "異常終了後の差分を確認中" : loaded ? "差分を確認中" : "Ready");
            catalog.ready.TrySetResult();
            catalog.Start();
            return catalog;
        }
        catch { catalog.DisposeFailedOpen(); throw; }
    }

    public CatalogSnapshot GetSnapshot()
    {
        ThrowIfDisposed();
        FilenameRecord[] records;
        lock (gate) records = byPath.Values.OrderBy(r => r.FileId).ToArray();
        long g = Generation;
        return new CatalogSnapshot(g, records, new Dictionary<string, long>(StringComparer.Ordinal) { [volumeId] = g });
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ThrowIfDisposed();
        engineGate.EnterReadLock();
        try { return engine.Search(request); }
        finally { engineGate.ExitReadLock(); }
    }

    public Task ReconcileAsync(CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        Interlocked.Exchange(ref forceReconcile, 1); SignalReconcile();
        return WaitForIdleAsync(TimeSpan.FromMinutes(10), cancellationToken);
    }

    public async Task WaitForIdleAsync(TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        cts.CancelAfter(timeout);
        while (true)
        {
            cts.Token.ThrowIfCancellationRequested();
            if (Status == "Ready" && Volatile.Read(ref pendingEvents) == 0 &&
                Volatile.Read(ref forceReconcile) == 0 && Volatile.Read(ref compactionRunning) == 0) return;
            await Task.Delay(20, cts.Token).ConfigureAwait(false);
        }
    }

    public static long PersistentBytes(string store) => GenerationStore.GetPersistentBytes(store);

    private void Start()
    {
        if (started) return;
        started = true; changeFeed.Start(); Queue(new FileSystemEvent(root, CatchUp: true));
    }

    private void Queue(FileSystemEvent change)
    {
        if (disposed) return;
        bool nowExcluded = IsExcluded(change.Path);
        bool oldExcluded = change.OldPath is not null && IsExcluded(change.OldPath);
        if (change.OldPath is null ? nowExcluded : nowExcluded && oldExcluded) return;
        if (change.Reconcile) Interlocked.Exchange(ref forceReconcile, 1);
        if (events.Writer.TryWrite(change))
        {
            int pending = Interlocked.Increment(ref pendingEvents);
            UpdateMaxPending(pending);
        }
        else
        {
            Interlocked.Increment(ref queueSaturationCount);
            Interlocked.Exchange(ref forceReconcile, 1); SignalReconcile();
        }
    }

    private void SignalReconcile()
    {
        if (events.Writer.TryWrite(new FileSystemEvent(root, Reconcile: true)))
        {
            int pending = Interlocked.Increment(ref pendingEvents);
            UpdateMaxPending(pending);
        }
        else Interlocked.Increment(ref queueSaturationCount);
    }

    private async Task UpdateLoop()
    {
        try
        {
            while (await events.Reader.WaitToReadAsync(stop.Token).ConfigureAwait(false))
            {
                await Task.Delay(50, stop.Token).ConfigureAwait(false);
                var batch = new List<FileSystemEvent>();
                while (events.Reader.TryRead(out FileSystemEvent? e)) { Interlocked.Decrement(ref pendingEvents); batch.Add(e); }
                try
                {
                    SetStatus("変更を反映中");
                    bool reconcile = Interlocked.Exchange(ref forceReconcile, 0) != 0 || batch.Any(e => e.Reconcile || e.CatchUp);
                    IReadOnlyList<CatalogChange> changes = reconcile ? ReconcileCore(stop.Token) : ProcessEvents(batch);
                    if (Interlocked.Exchange(ref forceReconcile, 0) != 0)
                        changes = changes.Concat(ReconcileCore(stop.Token)).ToArray();
                    Publish(changes, reconcile || batch.Any(e => e.Reconcile || e.CatchUp));
                    CompactIfNeeded();
                    SetStatus("Ready");
                }
                catch (OperationCanceledException) when (stop.IsCancellationRequested) { break; }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException)
                {
                    SetStatus("変更反映エラー: " + ex.Message);
                    Interlocked.Exchange(ref forceReconcile, 1); SignalReconcile();
                }
            }
        }
        catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
    }

    private IReadOnlyList<CatalogChange> ProcessEvents(IReadOnlyList<FileSystemEvent> pending)
    {
        var changes = new List<CatalogChange>();
        foreach (FileSystemEvent e in pending)
        {
            if (e.OldPath is not null) changes.AddRange(ProcessRename(e.OldPath, e.Path));
            else changes.AddRange(ProcessPath(e.Path, e.Kind));
        }
        return changes;
    }

    private IReadOnlyList<CatalogChange> ProcessRename(string oldPath, string newPath)
    {
        oldPath = Path.GetFullPath(oldPath); newPath = Path.GetFullPath(newPath);
        var changes = new List<CatalogChange>();
        lock (gate)
        {
            if (!byPath.TryGetValue(oldPath, out FilenameRecord? old)) { Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            if (old.IsDirectory || Directory.Exists(newPath)) { Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            byPath.Remove(oldPath); engine.Remove(old.FileId);
            if (IsExcluded(newPath)) { changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); return changes; }
            FilenameRecord? next = TryReadRecord(newPath, old.FileId);
            if (next is null) { changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            if (old.Key.IsNative && next.Key.IsNative && old.Key != next.Key)
            {
                next = AttachParent(next with { FileId = AllocateId() });
                byPath[newPath] = next; engine.Upsert(next);
                changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null));
                changes.Add(new(CatalogChangeKind.Added, next.Key, null, next));
                return changes;
            }
            if (old.Key.IsNative && !next.Key.IsNative) next = next with { Key = old.Key };
            next = AttachParent(next); byPath[newPath] = next; engine.Upsert(next);
            CatalogChangeKind kind = string.Equals(Path.GetDirectoryName(oldPath), Path.GetDirectoryName(newPath), StringComparison.OrdinalIgnoreCase)
                ? CatalogChangeKind.Renamed : CatalogChangeKind.Moved;
            changes.Add(new(kind, next.Key, old, next));
        }
        return changes;
    }

    private IReadOnlyList<CatalogChange> ProcessPath(string path, FileSystemEventKind kind)
    {
        path = Path.GetFullPath(path);
        var changes = new List<CatalogChange>();
        if (IsExcluded(path)) return changes;
        if (Directory.Exists(path)) { if (kind != FileSystemEventKind.Changed) Interlocked.Exchange(ref forceReconcile, 1); return changes; }
        lock (gate)
        {
            if (File.Exists(path))
            {
                byPath.TryGetValue(path, out FilenameRecord? old);
                FilenameRecord? next = TryReadRecord(path, old?.FileId ?? AllocateId());
                if (next is null) { Interlocked.Exchange(ref forceReconcile, 1); return changes; }
                if (old is not null && old.Key.IsNative && next.Key.IsNative && old.Key != next.Key)
                {
                    engine.Remove(old.FileId); next = AttachParent(next with { FileId = AllocateId() });
                    byPath[path] = next; engine.Upsert(next);
                    changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); changes.Add(new(CatalogChangeKind.Added, next.Key, null, next));
                    return changes;
                }
                if (old is not null && old.Key.IsNative && !next.Key.IsNative) next = next with { Key = old.Key };
                next = AttachParent(next);
                if (old is not null && Equivalent(old, next)) return changes;
                byPath[path] = next; engine.Upsert(next);
                changes.Add(new(old is null ? CatalogChangeKind.Added : CatalogChangeKind.Updated, next.Key, old, next));
            }
            else if (byPath.Remove(path, out FilenameRecord? removed))
            {
                engine.Remove(removed.FileId); changes.Add(new(CatalogChangeKind.Removed, removed.Key, removed, null));
                if (removed.IsDirectory) Interlocked.Exchange(ref forceReconcile, 1);
            }
        }
        return changes;
    }

    private void Publish(IReadOnlyList<CatalogChange> changes, bool reconciled)
    {
        if (changes.Count > 0)
        {
            long g = Interlocked.Increment(ref generation);
            var durable = new CatalogChangeBatch(g, changes, reconciled, volumeId, g);
            persistence.Append(durable);
            SafeRaise(durable);
        }
        else if (reconciled)
        {
            long g = Generation;
            SafeRaise(new CatalogChangeBatch(g, [], true, volumeId, g));
        }
    }

    private void CompactIfNeeded()
    {
        if (!engine.ShouldCompact && !persistence.ShouldCompact) return;
        if (Interlocked.Exchange(ref compactionRunning, 1) != 0) return;
        try
        {
            SetStatus("最適化中");
            FilenameSearchEngine compacted;
            engineGate.EnterReadLock(); try { compacted = engine.CreateCompacted(); } finally { engineGate.ExitReadLock(); }
            FilenameRecord[] snapshot; lock (gate) snapshot = byPath.Values.OrderBy(r => r.FileId).ToArray();
            persistence.CommitCompaction(compacted, snapshot, Generation);
            Interlocked.Increment(ref compactionCount);
            engineGate.EnterWriteLock();
            try { FilenameSearchEngine old = engine; engine = compacted; compacted = null!; old.Dispose(); }
            finally { engineGate.ExitWriteLock(); }
            compacted?.Dispose();
        }
        finally { Interlocked.Exchange(ref compactionRunning, 0); }
    }

    private void RebuildInitial()
    {
        FilenameRecord[] records = DiscoverRecords([], CancellationToken.None);
        engine.Build(records);
        lock (gate) { byPath.Clear(); foreach (FilenameRecord r in records) byPath[r.FullPath] = r; }
        generation = 1; persistence.InitializeBase(engine, records, generation);
    }

    private IReadOnlyList<CatalogChange> ReconcileCore(CancellationToken token)
    {
        Interlocked.Increment(ref reconcileCount);
        Dictionary<string, FilenameRecord> prior; lock (gate) prior = new(byPath, StringComparer.Ordinal);
        FilenameRecord[] records = DiscoverRecords(prior.Values, token);
        var next = records.ToDictionary(r => r.FullPath, StringComparer.Ordinal);
        IReadOnlyList<CatalogChange> changes = Diff(prior, next);
        if (changes.Count == 0) return changes;
        engineGate.EnterReadLock();
        try
        {
            var ids = next.Values.Select(r => r.FileId).ToHashSet();
            foreach (FilenameRecord old in prior.Values) if (!ids.Contains(old.FileId)) engine.Remove(old.FileId);
            foreach (FilenameRecord current in next.Values)
                if (!prior.TryGetValue(current.FullPath, out FilenameRecord? old) || !Equivalent(old, current)) engine.Upsert(current);
        }
        finally { engineGate.ExitReadLock(); }
        lock (gate) { byPath.Clear(); foreach ((string path, FilenameRecord r) in next) byPath[path] = r; }
        return changes;
    }

    private FilenameRecord[] DiscoverRecords(IEnumerable<FilenameRecord> priorRecords, CancellationToken token)
    {
        var prior = priorRecords.ToArray();
        var priorByPath = prior.ToDictionary(r => r.FullPath, StringComparer.Ordinal);
        var priorByKey = prior.GroupBy(r => r.Key).ToDictionary(g => g.Key, g => new Queue<FilenameRecord>(g.OrderBy(r => r.FileId)));
        var used = new HashSet<int>(); var reserved = prior.Select(r => r.FileId).ToHashSet();
        // Directory enumeration is the inexpensive part of a reconcile. File metadata and
        // native FileKey lookup are independent per entry, so collect them in parallel before
        // assigning stable FileIds. This keeps stopped-app catch-up bounded without changing
        // exact spelling, identity pairing, or the public feed contract.
        (string Path, bool IsDirectory)[] discovered = Discover(token).ToArray();
        var readRecords = new ConcurrentBag<(string Path, bool IsDirectory, FilenameRecord Record)>();
        Parallel.ForEach(discovered, new ParallelOptions
        {
            CancellationToken = token,
            MaxDegreeOfParallelism = Math.Max(4, Environment.ProcessorCount)
        }, entry =>
        {
            FilenameRecord? read = TryReadRecord(entry.Path, 0);
            if (read is not null) readRecords.Add((entry.Path, entry.IsDirectory, read));
        });
        var prelim = new List<FilenameRecord>(readRecords.Count);
        foreach ((string path, bool isDirectory, FilenameRecord read) in readRecords.OrderBy(item => item.Path, StringComparer.Ordinal))
        {
            int id;
            if (priorByPath.TryGetValue(path, out FilenameRecord? same)) id = same.FileId;
            else if (read.Key.IsNative && priorByKey.TryGetValue(read.Key, out Queue<FilenameRecord>? q))
            {
                while (q.Count > 0 && used.Contains(q.Peek().FileId)) q.Dequeue();
                id = q.Count > 0 ? q.Dequeue().FileId : AllocateId(reserved);
            }
            else id = AllocateId(reserved);
            used.Add(id); reserved.Add(id); prelim.Add(read with { FileId = id, Flags = isDirectory ? (byte)2 : (byte)1 });
        }
        var idsByPath = prelim.ToDictionary(r => r.FullPath, r => r, StringComparer.Ordinal);
        return prelim.Select(r =>
        {
            string? parent = Path.GetDirectoryName(r.FullPath);
            return parent is not null && idsByPath.TryGetValue(Path.GetFullPath(parent), out FilenameRecord? p)
                ? r with { ParentId = p.FileId, ParentKey = p.Key } : r with { ParentId = null, ParentKey = null };
        }).OrderBy(r => r.FileId).ToArray();
    }

    private IEnumerable<(string Path, bool IsDirectory)> Discover(CancellationToken token)
    {
        var stack = new Stack<string>(); stack.Push(root); yield return (root, true);
        while (stack.Count > 0)
        {
            token.ThrowIfCancellationRequested(); string dir = stack.Pop();
            string[] children;
            try { children = Directory.EnumerateFileSystemEntries(dir, "*", new EnumerationOptions { IgnoreInaccessible = true, AttributesToSkip = FileAttributes.ReparsePoint }).ToArray(); }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }
            foreach (string raw in children)
            {
                string path = Path.GetFullPath(raw); if (IsExcluded(path)) continue;
                FileAttributes attr; try { attr = File.GetAttributes(path); } catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }
                if (attr.HasFlag(FileAttributes.ReparsePoint)) continue;
                bool dirChild = attr.HasFlag(FileAttributes.Directory); yield return (path, dirChild); if (dirChild) stack.Push(path);
            }
        }
    }

    private FilenameRecord? TryReadRecord(string path, int id)
    {
        path = Path.GetFullPath(path);
        try
        {
            FileAttributes attr = File.GetAttributes(path); bool isDirectory = attr.HasFlag(FileAttributes.Directory);
            FileSystemInfo info = isDirectory ? new DirectoryInfo(path) : new FileInfo(path); info.Refresh();
            ulong size = isDirectory ? 0 : checked((ulong)((FileInfo)info).Length);
            return new FilenameRecord(id, null, info.Name, path, size, info.LastWriteTimeUtc, isDirectory ? (byte)2 : (byte)1)
            { Key = VolumeIdentity.GetFileKey(path, isDirectory, volumeId) };
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { return null; }
    }

    private FilenameRecord AttachParent(FilenameRecord record)
    {
        string? parent = Path.GetDirectoryName(record.FullPath);
        return parent is not null && byPath.TryGetValue(Path.GetFullPath(parent), out FilenameRecord? p)
            ? record with { ParentId = p.FileId, ParentKey = p.Key } : record with { ParentId = null, ParentKey = null };
    }

    private static IReadOnlyList<CatalogChange> Diff(
        IReadOnlyDictionary<string, FilenameRecord> prior, IReadOnlyDictionary<string, FilenameRecord> next)
    {
        var result = new List<CatalogChange>();
        foreach ((string path, FilenameRecord current) in next)
            if (prior.TryGetValue(path, out FilenameRecord? old) && !Equivalent(old, current))
                result.Add(new(CatalogChangeKind.Updated, current.Key, old, current));
        var removed = prior.Where(p => !next.ContainsKey(p.Key)).Select(p => p.Value).ToList();
        var added = next.Where(p => !prior.ContainsKey(p.Key)).Select(p => p.Value).ToList();
        var addedByKey = added.GroupBy(r => r.Key).ToDictionary(g => g.Key, g => new Queue<FilenameRecord>(g));
        var pairedAdded = new HashSet<int>();
        foreach (FilenameRecord old in removed)
        {
            if (old.Key.IsNative && addedByKey.TryGetValue(old.Key, out Queue<FilenameRecord>? q) && q.Count > 0)
            {
                FilenameRecord current = q.Dequeue(); pairedAdded.Add(current.FileId);
                CatalogChangeKind kind = string.Equals(Path.GetDirectoryName(old.FullPath), Path.GetDirectoryName(current.FullPath), StringComparison.OrdinalIgnoreCase)
                    ? CatalogChangeKind.Renamed : CatalogChangeKind.Moved;
                result.Add(new(kind, current.Key, old, current));
            }
            else result.Add(new(CatalogChangeKind.Removed, old.Key, old, null));
        }
        foreach (FilenameRecord current in added) if (!pairedAdded.Contains(current.FileId)) result.Add(new(CatalogChangeKind.Added, current.Key, null, current));
        return result;
    }

    private void LoadExistingRecords()
    {
        lock (gate)
        {
            byPath.Clear();
            foreach (FilenameRecord r in engine.Records)
            {
                if (!PathIdentity.IsSameOrChild(root, r.FullPath) || IsExcluded(r.FullPath)) continue;
                byPath[r.FullPath] = r; nextId = Math.Max(nextId, r.FileId == int.MaxValue ? int.MaxValue : r.FileId + 1);
            }
        }
    }

    private void UpdateMaxPending(int pending)
    {
        while (true)
        {
            int current = Volatile.Read(ref maxPendingEvents);
            if (pending <= current || Interlocked.CompareExchange(ref maxPendingEvents, pending, current) == current) return;
        }
    }

    private int AllocateId(HashSet<int>? reserved = null)
    {
        while (nextId <= 0 || reserved?.Contains(nextId) == true) { if (nextId == int.MaxValue) throw new InvalidOperationException("File ID space exhausted"); nextId++; }
        return nextId++;
    }
    private bool IsExcluded(string path) => excludedRoots.Any(x => PathIdentity.IsSameOrChild(x, path)) || PathIdentity.IsSameOrChild(store + ".data", path) || path.Equals(store, StringComparison.OrdinalIgnoreCase);
    private static bool Equivalent(FilenameRecord a, FilenameRecord b) => a.FileId == b.FileId && a.Key == b.Key && a.ParentId == b.ParentId && a.ParentKey == b.ParentKey && a.Name == b.Name && a.FullPath == b.FullPath && a.SizeBytes == b.SizeBytes && a.ModifiedUtc == b.ModifiedUtc && a.Flags == b.Flags;
    private void SafeRaise(CatalogChangeBatch batch) { try { Changed?.Invoke(batch); } catch { } }
    private void SetStatus(string value) => Volatile.Write(ref status, value);
    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);

    private void DisposeFailedOpen()
    {
        disposed = true; try { changeFeed.Dispose(); } catch { } try { persistence.Dispose(); } catch { } try { engine.Dispose(); } catch { } stop.Cancel(); events.Writer.TryComplete();
    }

    public async ValueTask DisposeAsync()
    {
        if (disposed) return; disposed = true;
        try { changeFeed.Stop(); } catch { } changeFeed.Dispose();
        stop.Cancel(); events.Writer.TryComplete();
        try { await worker.ConfigureAwait(false); } catch (OperationCanceledException) { }
        persistence.MarkClean(); persistence.Dispose();
        engineGate.EnterWriteLock(); try { engine.Dispose(); } finally { engineGate.ExitWriteLock(); engineGate.Dispose(); }
        stop.Dispose();
    }
}

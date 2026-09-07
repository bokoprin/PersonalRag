using System.Collections.Concurrent;
using System.IO.Enumeration;
using System.Threading.Channels;

namespace PersonalRag.FilenameSearch;

internal enum FileSystemEventKind { Created, Changed, Deleted, Renamed }
internal sealed record FileSystemEvent(
    string Path, string? OldPath = null, FileSystemEventKind Kind = FileSystemEventKind.Changed,
    bool Reconcile = false, bool CatchUp = false);

/// <summary>One-volume exact-metadata catalog with bounded change ingestion and durable generation deltas.</summary>
public sealed class FileSystemCatalog : IFilenameCatalog
{
    private readonly record struct ReadMetadata(
        string FullPath, string Name, ulong SizeBytes, DateTime ModifiedUtc, byte Flags, FileKey Key);
    private readonly record struct DiscoveredEntry(
        string FullPath, string Name, ulong SizeBytes, DateTime ModifiedUtc, bool IsDirectory);

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
    // Steady state keeps only a compact path-hash -> FileId map. Exact names/paths live in
    // FilenameSearchEngine's UTF-8 metadata table and are materialized only for candidates.
    private readonly Dictionary<ulong, int> pathIds = [];
    private readonly Dictionary<ulong, List<int>> pathCollisions = [];
    private readonly Dictionary<FileKey, (FilenameRecord Record, DateTime ExpiresUtc)> pendingDeletes = [];
    private readonly ConcurrentDictionary<string, byte> metadataRetries = new(StringComparer.OrdinalIgnoreCase);
    // The immutable base uses sorted flat arrays instead of a million-entry Dictionary.
    // pathIds/pathCollisions are reserved for the small live overlay and tombstones.
    private ulong[] basePathHashes = [];
    private int[] basePathIds = [];
    private readonly Dictionary<int, FilenameRecord> mutableRecords = [];
    private Task basePathIndexReady = Task.CompletedTask;
    private int recordCount;
    private FilenameSearchEngine engine;
    private int nextId = 1, pendingEvents, maxPendingEvents, forceReconcile, reconcileQueued, compactionRunning, directoryReconcileScheduled;
    private long lastEventTick = Environment.TickCount64;
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
        changeFeed = VolumeChangeFeedFactory.Create(this.root, this.store);
        changeFeed.Changed += Queue;
        changeFeed.Overflow += () =>
        {
            Volatile.Write(ref lastEventTick, Environment.TickCount64);
            Interlocked.Exchange(ref forceReconcile, 1);
            SignalReconcile();
        };
        worker = Task.Run(UpdateLoop);
    }

    public string Root => root;
    public string Store => store;
    public string RootIdentity => rootIdentity;
    public string VolumeId => volumeId;
    public string Status => Volatile.Read(ref status);
    public long Generation => Interlocked.Read(ref generation);
    public int RecordCount { get { lock (gate) return recordCount; } }
    public bool IsReady => ready.Task.IsCompletedSuccessfully;
    public Task Ready => ready.Task;
    public bool WasDirtyShutdown => persistence.WasDirtyShutdown;
    public event Action<CatalogChangeBatch>? Changed;
    public IReadOnlyList<FilenameRecord> Records { get { lock (gate) return engine.Records; } }

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
        lock (gate) return TryGetPathLocked(full, out _);
    }

    internal bool TryGetRecordAtPath(string path, out FilenameRecord? record)
    {
        string full = Path.GetFullPath(path);
        lock (gate) return TryGetPathLocked(full, out record);
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
                if (loaded)
                {
                    catalog.generation = persisted;
                    catalog.LoadExistingRecords();
                }
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
        lock (gate) records = engine.Records.OrderBy(r => r.FileId).ToArray();
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
                Volatile.Read(ref forceReconcile) == 0 && Volatile.Read(ref compactionRunning) == 0)
            {
                // Existing stores publish Ready before the asynchronous compact path index
                // is available.  A caller that starts querying immediately after Ready
                // would then contend with the million-entry hash copy in ForEachPathHash.
                // Treat that one-time index build as part of idle so startup probes and live
                // consumers observe a genuinely settled catalog.
                await basePathIndexReady.WaitAsync(cts.Token).ConfigureAwait(false);
                if (Status == "Ready" && Volatile.Read(ref pendingEvents) == 0 &&
                    Volatile.Read(ref forceReconcile) == 0 && Volatile.Read(ref compactionRunning) == 0)
                    return;
            }
            await Task.Delay(20, cts.Token).ConfigureAwait(false);
        }
    }

    public static long PersistentBytes(string store) => GenerationStore.GetPersistentBytes(store);

    private void Start()
    {
        if (started) return;
        started = true;
        // The watcher backend does not need a catalog snapshot. Avoid materializing a
        // million-record graph merely to pass it to its no-op SetSnapshot implementation;
        // the USN backend opts in because it resolves native identities during catch-up.
        if (changeFeed.RequiresSnapshot)
            changeFeed.SetSnapshot(visitor => engine.ForEachNativePath(visitor));
        changeFeed.Start();
        if (!changeFeed.FastCatchUpAvailable) Queue(new FileSystemEvent(root, CatchUp: true));
    }

    private void Queue(FileSystemEvent change)
    {
        if (disposed) return;
        bool nowExcluded = IsExcluded(change.Path);
        bool oldExcluded = change.OldPath is not null && IsExcluded(change.OldPath);
        if (change.OldPath is null ? nowExcluded : nowExcluded && oldExcluded) return;
        Volatile.Write(ref lastEventTick, Environment.TickCount64);
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
        if (Interlocked.Exchange(ref reconcileQueued, 1) != 0) return;
        if (events.Writer.TryWrite(new FileSystemEvent(root, Reconcile: true)))
        {
            int pending = Interlocked.Increment(ref pendingEvents);
            UpdateMaxPending(pending);
        }
        else
        {
            Interlocked.Exchange(ref reconcileQueued, 0);
            Interlocked.Increment(ref queueSaturationCount);
        }
    }

    private async Task UpdateLoop()
    {
        try
        {
            await basePathIndexReady.ConfigureAwait(false);
            while (await events.Reader.WaitToReadAsync(stop.Token).ConfigureAwait(false))
            {
                await Task.Delay(50, stop.Token).ConfigureAwait(false);
                var batch = new List<FileSystemEvent>();
                while (events.Reader.TryRead(out FileSystemEvent? e)) { Interlocked.Decrement(ref pendingEvents); batch.Add(e); }
                if (batch.Any(e => e.Reconcile || e.CatchUp)) Interlocked.Exchange(ref reconcileQueued, 0);
                try
                {
                    SetStatus("変更を反映中");
                    bool reconcile = Interlocked.Exchange(ref forceReconcile, 0) != 0 || batch.Any(e => e.Reconcile || e.CatchUp);
                    IReadOnlyList<CatalogChange> changes;
                    if (reconcile)
                    {
                        // A delayed directory reconcile can share a batch with ordinary
                        // child events. Apply those direct events first and publish them
                        // before the authoritative scan; otherwise a live consumer would
                        // wait for the full 1M-entry reconcile even though the changed file
                        // metadata is already available.
                        FileSystemEvent[] direct = batch.Where(e => !e.Reconcile && !e.CatchUp).ToArray();
                        if (direct.Length != 0)
                        {
                            IReadOnlyList<CatalogChange> directChanges = ProcessEvents(direct);
                            if (directChanges.Count != 0) Publish(directChanges, reconciled: false);
                        }
                        // A saturated watcher queue already means the filesystem scan is
                        // the authoritative event set. Drain markers/events and wait for a
                        // short quiet window so one storm cannot retain a million-record
                        // reconcile graph on every worker iteration.
                        await WaitForReconcileQuietAsync(stop.Token).ConfigureAwait(false);
                        Interlocked.Exchange(ref forceReconcile, 0);
                        changes = ReconcileCore(stop.Token);
                    }
                    else changes = ProcessEvents(batch);
                    if (Interlocked.Exchange(ref forceReconcile, 0) != 0) SignalReconcile();
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

    private async Task WaitForReconcileQuietAsync(CancellationToken cancellationToken)
    {
        const long quietMilliseconds = 250;
        while (true)
        {
            while (events.Reader.TryRead(out _)) Interlocked.Decrement(ref pendingEvents);
            long quietFor = Environment.TickCount64 - Volatile.Read(ref lastEventTick);
            if (Volatile.Read(ref pendingEvents) == 0 && quietFor >= quietMilliseconds) return;
            await Task.Delay((int)Math.Clamp(quietMilliseconds - quietFor, 10, quietMilliseconds), cancellationToken)
                .ConfigureAwait(false);
        }
    }

    private IReadOnlyList<CatalogChange> ProcessEvents(IReadOnlyList<FileSystemEvent> pending)
    {
        var changes = new List<CatalogChange>();
        var deletedByKey = new Dictionary<FileKey, Queue<(int Index, FilenameRecord Record)>>();
        for (int i = 0; i < pending.Count; i++)
        {
            FileSystemEvent e = pending[i];
            if (e.Kind != FileSystemEventKind.Deleted || e.OldPath is not null) continue;
            lock (gate)
            {
                if (TryGetPathLocked(e.Path, out FilenameRecord? old) && old!.Key.IsNative)
                {
                    if (!deletedByKey.TryGetValue(old.Key, out Queue<(int, FilenameRecord)>? queue))
                        deletedByKey[old.Key] = queue = new();
                    queue.Enqueue((i, old));
                }
            }
        }
        var pairedDeletes = new HashSet<int>();
        var pairedCreates = new Dictionary<int, FilenameRecord>();
        for (int i = 0; i < pending.Count; i++)
        {
            FileSystemEvent e = pending[i];
            if (e.Kind != FileSystemEventKind.Created || e.OldPath is not null) continue;
            FilenameRecord? next = TryReadRecord(e.Path, 0);
            if (next is null || !next.Key.IsNative || !deletedByKey.TryGetValue(next.Key, out Queue<(int, FilenameRecord)>? queue) || queue.Count == 0)
                continue;
            (int deletedIndex, FilenameRecord old) = queue.Dequeue();
            pairedDeletes.Add(deletedIndex);
            pairedCreates[i] = old;
        }

        for (int i = 0; i < pending.Count; i++)
        {
            if (pairedDeletes.Contains(i)) continue;
            FileSystemEvent e = pending[i];
            if (pairedCreates.TryGetValue(i, out FilenameRecord? old))
            {
                changes.AddRange(ProcessRename(old.FullPath, e.Path));
                continue;
            }
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
            if (!TryGetPathLocked(oldPath, out FilenameRecord? old)) { Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            if (old!.IsDirectory || Directory.Exists(newPath)) { Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            RemovePathLocked(oldPath, old.FileId); engine.Remove(old.FileId); mutableRecords.Remove(old.FileId);
            if (IsExcluded(newPath)) { changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); return changes; }
            FilenameRecord? next = TryReadRecord(newPath, old.FileId);
            if (next is null) { changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); Interlocked.Exchange(ref forceReconcile, 1); return changes; }
            if (old.Key.IsNative && next.Key.IsNative && old.Key != next.Key)
            {
                next = AttachParent(next with { FileId = AllocateId() });
                AddPathLocked(next); mutableRecords[next.FileId] = next; engine.Upsert(next);
                changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null));
                changes.Add(new(CatalogChangeKind.Added, next.Key, null, next));
                return changes;
            }
            if (old.Key.IsNative && !next.Key.IsNative) next = next with { Key = old.Key };
            next = AttachParent(next); AddPathLocked(next); mutableRecords[next.FileId] = next; engine.Upsert(next);
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
        if (Directory.Exists(path))
        {
            // A newly-created directory is itself a complete metadata record. Publish it
            // immediately so live consumers do not wait for a million-entry reconcile just
            // because the watcher also reports the directory boundary.  A delayed reconcile
            // still covers children whose notifications were coalesced or dropped.
            if (kind == FileSystemEventKind.Changed) return changes;
            lock (gate)
            {
                TryGetPathLocked(path, out FilenameRecord? old);
                FilenameRecord? next = TryReadRecord(path, old?.FileId ?? AllocateId());
                if (next is null)
                {
                    ScheduleMetadataRetry(path, kind);
                    return changes;
                }
                next = AttachParent(next);
                if (old is not null && Equivalent(old, next))
                {
                    ScheduleDirectoryReconcile();
                    return changes;
                }
                AddPathLocked(next);
                mutableRecords[next.FileId] = next;
                engine.Upsert(next);
                changes.Add(new(old is null ? CatalogChangeKind.Added : CatalogChangeKind.Updated,
                    next.Key, old, next));
            }
            ScheduleDirectoryReconcile();
            return changes;
        }
        lock (gate)
        {
            if (File.Exists(path))
            {
                TryGetPathLocked(path, out FilenameRecord? old);
                FilenameRecord? next = TryReadRecord(path, old?.FileId ?? AllocateId());
                if (next is null) { ScheduleMetadataRetry(path, kind); return changes; }
                if (old is null && next.Key.IsNative && TryTakePendingDelete(next.Key, out FilenameRecord? movedFrom))
                {
                    next = AttachParent(next with { FileId = movedFrom!.FileId });
                    AddPathLocked(next); mutableRecords[next.FileId] = next; engine.Upsert(next);
                    changes.Add(new(CatalogChangeKind.Moved, next.Key, movedFrom, next));
                    return changes;
                }
                if (old is not null && old.Key.IsNative && next.Key.IsNative && old.Key != next.Key)
                {
                    engine.Remove(old.FileId); next = AttachParent(next with { FileId = AllocateId() });
                    AddPathLocked(next); mutableRecords[next.FileId] = next; engine.Upsert(next);
                    changes.Add(new(CatalogChangeKind.Removed, old.Key, old, null)); changes.Add(new(CatalogChangeKind.Added, next.Key, null, next));
                    return changes;
                }
                if (old is not null && old.Key.IsNative && !next.Key.IsNative) next = next with { Key = old.Key };
                next = AttachParent(next);
                if (old is not null && Equivalent(old, next)) return changes;
                AddPathLocked(next); mutableRecords[next.FileId] = next; engine.Upsert(next);
                changes.Add(new(old is null ? CatalogChangeKind.Added : CatalogChangeKind.Updated, next.Key, old, next));
            }
            else if (TryGetPathLocked(path, out FilenameRecord? removed))
            {
                if (removed!.Key.IsNative) pendingDeletes[removed.Key] = (removed, DateTime.UtcNow.AddSeconds(2));
                RemovePathLocked(path, removed.FileId); mutableRecords.Remove(removed.FileId); engine.Remove(removed.FileId); changes.Add(new(CatalogChangeKind.Removed, removed.Key, removed, null));
                if (removed.IsDirectory) Interlocked.Exchange(ref forceReconcile, 1);
            }
            else if (kind == FileSystemEventKind.Created)
            {
                // A cross-directory move can surface as Deleted + Created, and
                // the Created notification may arrive before the destination is
                // openable.  Do not drop that event; bounded reconcile pairs the
                // native FileKey and publishes the move once the entry settles.
                ScheduleMetadataRetry(path, kind);
            }
        }
        return changes;
    }

    private void ScheduleMetadataRetry(string path, FileSystemEventKind kind)
    {
        path = Path.GetFullPath(path);
        if (!metadataRetries.TryAdd(path, 0)) return;
        _ = Task.Run(async () =>
        {
            try
            {
                for (int attempt = 0; attempt < 6; attempt++)
                {
                    await Task.Delay(50, stop.Token).ConfigureAwait(false);
                    if (disposed) return;
                    if (File.Exists(path) || Directory.Exists(path))
                    {
                        Queue(new FileSystemEvent(path, Kind: kind));
                        return;
                    }
                }
                if (!disposed)
                {
                    Interlocked.Exchange(ref forceReconcile, 1);
                    SignalReconcile();
                }
            }
            catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
            finally { metadataRetries.TryRemove(path, out _); }
        });
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
            FilenameRecord[] snapshot; lock (gate) snapshot = engine.Records.OrderBy(r => r.FileId).ToArray();
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
        IReadOnlyList<FilenameRecord> records = DiscoverRecords([], CancellationToken.None);
        engine.Build(records);
        lock (gate)
        {
            ClearPathMapsLocked();
            BuildBasePathIndexLocked(records, records.Count);
            nextId = records.Count == 0 ? 1 : records.Max(record =>
                record.FileId == int.MaxValue ? int.MaxValue : record.FileId + 1);
        }
        generation = 1; persistence.InitializeBase(engine, records, generation);
    }

    private IReadOnlyList<CatalogChange> ReconcileCore(CancellationToken token)
    {
        Interlocked.Increment(ref reconcileCount);
        // A quiet restart usually has the same path set and only a small number of
        // metadata changes.  Check the compact path index directly so a saturated
        // watcher queue does not materialize a million-entry prior dictionary merely
        // to discover that every path is still present.  The full reconcile remains
        // the fallback whenever a path is added, removed, renamed, or moved.
        if (TryApplyStableMetadataUpdates(token, out IReadOnlyList<CatalogChange> stableChanges))
            return stableChanges;
        Dictionary<string, FilenameRecord> prior = engine.SnapshotByPath();
        (Dictionary<string, FilenameRecord> next, bool changed) = ScanReconcile(prior, token);
        IReadOnlyList<CatalogChange> changes = changed ? Diff(prior, next) : [];
        if (changes.Count == 0) return changes;
        engineGate.EnterWriteLock();
        try
        {
            var ids = next.Values.Select(r => r.FileId).ToHashSet();
            foreach (FilenameRecord old in prior.Values) if (!ids.Contains(old.FileId)) engine.Remove(old.FileId);
            foreach (FilenameRecord current in next.Values)
                if (!prior.TryGetValue(current.FullPath, out FilenameRecord? old) || !Equivalent(old, current)) engine.Upsert(current);
        }
        finally { engineGate.ExitWriteLock(); }
        lock (gate)
        {
            ClearPathMapsLocked();
            BuildBasePathIndexLocked(next.Values, next.Count);
        }
        return changes;
    }

    private bool TryTakePendingDelete(FileKey key, out FilenameRecord? record)
    {
        DateTime now = DateTime.UtcNow;
        if (!pendingDeletes.TryGetValue(key, out (FilenameRecord Record, DateTime ExpiresUtc) pending))
        {
            record = null;
            return false;
        }
        if (pending.ExpiresUtc <= now)
        {
            pendingDeletes.Remove(key);
            record = null;
            return false;
        }
        pendingDeletes.Remove(key);
        record = pending.Record;
        return true;
    }

    private void ScheduleDirectoryReconcile()
    {
        if (Interlocked.Exchange(ref directoryReconcileScheduled, 1) != 0) return;
        // Give child notifications a chance to arrive and be published directly.  The
        // timer is measured from the last filesystem event rather than from the directory
        // event itself, so a create/rename/delete sequence cannot be interrupted by the
        // fallback 1M scan.  If notifications are missing, the quiet window still expires
        // and the authoritative reconcile restores the subtree.
        _ = Task.Run(async () =>
        {
            try
            {
                const int quietMilliseconds = 10_000;
                while (true)
                {
                    long quietFor = Environment.TickCount64 - Volatile.Read(ref lastEventTick);
                    int delay = (int)Math.Clamp(quietMilliseconds - quietFor, 100, quietMilliseconds);
                    await Task.Delay(delay, stop.Token).ConfigureAwait(false);
                    if (Volatile.Read(ref pendingEvents) != 0) continue;
                    quietFor = Environment.TickCount64 - Volatile.Read(ref lastEventTick);
                    if (quietFor < quietMilliseconds) continue;
                    Interlocked.Exchange(ref directoryReconcileScheduled, 0);
                    if (!disposed)
                    {
                        Interlocked.Exchange(ref forceReconcile, 1);
                        SignalReconcile();
                    }
                    return;
                }
            }
            catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
        });
    }

    private bool TryApplyStableMetadataUpdates(CancellationToken token, out IReadOnlyList<CatalogChange> changes)
    {
        var changeList = new List<CatalogChange>();
        changes = changeList;
        int currentCount;
        lock (gate) currentCount = recordCount;

        // FileIds are stable but may contain holes after deletes/moves.  A compact
        // bitset keyed by the highest live id lets us detect missing entries without
        // allocating a path dictionary.  The normal production corpus has dense ids,
        // and the fallback below still handles pathological/sparse stores safely.
        int maxId = engine.MaxFileId;
        if (maxId == int.MaxValue) return false;
        var seen = new bool[checked(maxId + 1)];
        var updates = new List<(FilenameRecord Old, FilenameRecord Current)>();
        int seenCount = 0;
        // Use the single-pass enumerator here as well as during initial build.  The
        // parallel Discover implementation retains every entry in a ConcurrentBag
        // until enumeration completes, which is unnecessary for a path-presence check
        // and makes the post-storm memory sample include that transient million-entry
        // graph.
        foreach (DiscoveredEntry entry in DiscoverStreaming(token))
        {
            FilenameRecord? old;
            lock (gate)
            {
                if (!TryGetPathLocked(entry.FullPath, out old) || old is null) return false;
            }
            if (old.FileId >= seen.Length) return false;
            if (!seen[old.FileId]) { seen[old.FileId] = true; seenCount++; }

            byte flags = entry.IsDirectory ? (byte)2 : (byte)1;
            if (old.Name == entry.Name && old.FullPath == entry.FullPath &&
                old.SizeBytes == entry.SizeBytes && old.ModifiedUtc == entry.ModifiedUtc && old.Flags == flags)
                continue;

            ReadMetadata? read = TryReadMetadata(entry, old);
            if (read is null) return false;
            FilenameRecord current = new(old.FileId, old.ParentId, read.Value.Name, read.Value.FullPath,
                read.Value.SizeBytes, read.Value.ModifiedUtc, read.Value.Flags)
            { Key = read.Value.Key, ParentKey = old.ParentKey };
            if (!Equivalent(old, current)) updates.Add((old, current));
        }

        if (seenCount != currentCount) return false;
        if (updates.Count == 0) return true;

        engineGate.EnterWriteLock();
        try
        {
            lock (gate)
            {
                foreach ((FilenameRecord old, FilenameRecord current) in updates)
                {
                    engine.Upsert(current);
                    mutableRecords[current.FileId] = current;
                    changeList.Add(new(CatalogChangeKind.Updated, current.Key, old, current));
                }
            }
        }
        finally { engineGate.ExitWriteLock(); }
        return true;
    }

    private (Dictionary<string, FilenameRecord> Next, bool Changed) ScanReconcile(
        IReadOnlyDictionary<string, FilenameRecord> prior, CancellationToken token)
    {
        // Startup/reconcile must keep the parallel metadata reads that make catch-up fast,
        // but it must process the filesystem in bounded batches. Holding a million-entry
        // discovery array and a million-entry read array together makes a quiet restart
        // consume gigabytes before the unchanged records can be reused.
        var next = new Dictionary<string, FilenameRecord>(Math.Max(prior.Count, 4_096), StringComparer.Ordinal);
        var batch = new List<DiscoveredEntry>(4_096);
        bool changed = false;
        foreach (DiscoveredEntry entry in Discover(token))
        {
            batch.Add(entry);
            if (batch.Count == batch.Capacity)
            {
                ProcessReconcileBatch(batch, prior, next, ref changed, token);
                batch.Clear();
            }
        }
        if (batch.Count != 0) ProcessReconcileBatch(batch, prior, next, ref changed, token);

        // Preserve FileId across stopped renames/moves by pairing only the changed subset.
        // The common unchanged 1M startup path therefore avoids a million-entry key queue.
        var removed = prior.Where(pair => !next.ContainsKey(pair.Key)).Select(pair => pair.Value).ToList();
        var added = next.Where(pair => !prior.ContainsKey(pair.Key)).Select(pair => pair.Value).ToList();
        changed |= removed.Count != 0 || added.Count != 0;
        var addedByKey = added.GroupBy(record => record.Key)
            .ToDictionary(group => group.Key, group => new Queue<FilenameRecord>(group));
        foreach (FilenameRecord old in removed)
        {
            if (!old.Key.IsNative || !addedByKey.TryGetValue(old.Key, out Queue<FilenameRecord>? queue) || queue.Count == 0)
                continue;
            FilenameRecord current = queue.Dequeue();
            next[current.FullPath] = current with { FileId = old.FileId };
        }

        // The two temporary arrays contain only references/records and are no longer needed
        // once the path map is complete; the caller publishes this map atomically below.
        return (next, changed);
    }

    private void ProcessReconcileBatch(
        IReadOnlyList<DiscoveredEntry> batch,
        IReadOnlyDictionary<string, FilenameRecord> prior,
        IDictionary<string, FilenameRecord> next,
        ref bool changed,
        CancellationToken token)
    {
        var reads = new ReadMetadata?[batch.Count];
        Parallel.For(0, batch.Count, new ParallelOptions
        {
            CancellationToken = token,
            MaxDegreeOfParallelism = Math.Max(4, Environment.ProcessorCount)
        }, i =>
        {
            prior.TryGetValue(batch[i].FullPath, out FilenameRecord? old);
            reads[i] = TryReadMetadata(batch[i], old);
        });
        for (int i = 0; i < reads.Length; i++)
        {
            ReadMetadata? read = reads[i];
            if (read is null) continue;
            string path = read.Value.FullPath;
            if (prior.TryGetValue(path, out FilenameRecord? old))
            {
                if (SameFilesystemMetadata(old, read.Value))
                {
                    next[path] = old;
                }
                else
                {
                    changed = true;
                    FilenameRecord current = ToRecord(read.Value, old.FileId);
                    next[path] = AttachParentTo(current, next);
                }
            }
            else
            {
                changed = true;
                FilenameRecord current = ToRecord(read.Value, AllocateId());
                next[path] = AttachParentTo(current, next);
            }
        }
    }

    private FilenameRecord AttachParentTo(FilenameRecord record, IDictionary<string, FilenameRecord> current)
    {
        string? parent = Path.GetDirectoryName(record.FullPath);
        return parent is not null && current.TryGetValue(Path.GetFullPath(parent), out FilenameRecord? p)
            ? record with { ParentId = p.FileId, ParentKey = p.Key }
            : record with { ParentId = null, ParentKey = null };
    }

    private static bool SameFilesystemMetadata(FilenameRecord a, ReadMetadata b)
    {
        bool same = a.Name == b.Name && a.FullPath == b.FullPath && a.Key == b.Key &&
            a.SizeBytes == b.SizeBytes && a.ModifiedUtc == b.ModifiedUtc && a.Flags == b.Flags;
        return same;
    }

    private static FilenameRecord ToRecord(ReadMetadata read, int id) => new(
        id, null, read.Name, read.FullPath, read.SizeBytes, read.ModifiedUtc, read.Flags) { Key = read.Key };

    private IReadOnlyList<FilenameRecord> DiscoverRecords(IEnumerable<FilenameRecord> priorRecords, CancellationToken token)
    {
        var prior = priorRecords.ToArray();
        if (prior.Length == 0) return DiscoverInitialRecords(token);
        var priorByPath = prior.ToDictionary(r => r.FullPath, StringComparer.Ordinal);
        var priorByKey = prior.GroupBy(r => r.Key).ToDictionary(g => g.Key, g => new Queue<FilenameRecord>(g.OrderBy(r => r.FileId)));
        var used = new HashSet<int>(); var reserved = prior.Select(r => r.FileId).ToHashSet();
        // Directory enumeration is the inexpensive part of a reconcile. File metadata and
        // native FileKey lookup are independent per entry, so collect them in parallel before
        // assigning stable FileIds. This keeps stopped-app catch-up bounded without changing
        // exact spelling, identity pairing, or the public feed contract.
        DiscoveredEntry[] discovered = Discover(token).ToArray();
        var readRecords = new ConcurrentBag<(DiscoveredEntry Entry, FilenameRecord Record)>();
        Parallel.ForEach(discovered, new ParallelOptions
        {
            CancellationToken = token,
            MaxDegreeOfParallelism = Math.Max(4, Environment.ProcessorCount)
        }, entry =>
        {
            FilenameRecord? read = TryReadRecord(entry, 0);
            if (read is not null) readRecords.Add((entry, read));
        });
        var prelim = new List<FilenameRecord>(readRecords.Count);
        foreach ((DiscoveredEntry entry, FilenameRecord read) in readRecords.OrderBy(item => item.Entry.FullPath, StringComparer.Ordinal))
        {
            int id;
            if (priorByPath.TryGetValue(entry.FullPath, out FilenameRecord? same)) id = same.FileId;
            else if (read.Key.IsNative && priorByKey.TryGetValue(read.Key, out Queue<FilenameRecord>? q))
            {
                while (q.Count > 0 && used.Contains(q.Peek().FileId)) q.Dequeue();
                id = q.Count > 0 ? q.Dequeue().FileId : AllocateId(reserved);
            }
            else id = AllocateId(reserved);
            used.Add(id); reserved.Add(id); prelim.Add(read with { FileId = id, Flags = entry.IsDirectory ? (byte)2 : (byte)1 });
        }
        var idsByPath = prelim.ToDictionary(r => r.FullPath, r => r, StringComparer.Ordinal);
        return prelim.Select(r =>
        {
            string? parent = Path.GetDirectoryName(r.FullPath);
            return parent is not null && idsByPath.TryGetValue(Path.GetFullPath(parent), out FilenameRecord? p)
                ? r with { ParentId = p.FileId, ParentKey = p.Key } : r with { ParentId = null, ParentKey = null };
        }).OrderBy(r => r.FileId).ToArray();
    }

    /// <summary>
    /// Initial build path. The previous implementation materialized the complete discovery
    /// graph, a parallel read bag, a preliminary record list, and a second path dictionary at
    /// the same time. On a million-entry corpus that transient graph exceeded the steady
    /// memory gate before the compact indexes were even built. This path keeps enumeration
    /// and metadata reads bounded while preserving exact spelling, native FileKeys, and
    /// parent/FileId identity.
    /// </summary>
    private IReadOnlyList<FilenameRecord> DiscoverInitialRecords(CancellationToken token)
    {
        const int batchSize = 4_096;
        var records = new List<FilenameRecord>();
        var idsByPath = new Dictionary<string, int>(StringComparer.Ordinal);
        var batch = new List<DiscoveredEntry>(batchSize);
        int next = 1;

        foreach (DiscoveredEntry entry in DiscoverStreaming(token))
        {
            batch.Add(entry);
            if (batch.Count == batchSize) ProcessInitialBatch();
        }
        if (batch.Count != 0) ProcessInitialBatch();
        idsByPath.Clear();
        return records;

        void ProcessInitialBatch()
        {
            var reads = new ReadMetadata?[batch.Count];
            Parallel.For(0, batch.Count, new ParallelOptions
            {
                CancellationToken = token,
                MaxDegreeOfParallelism = Math.Max(4, Environment.ProcessorCount)
            }, i => reads[i] = TryReadMetadata(batch[i], null));
            for (int i = 0; i < reads.Length; i++)
            {
                ReadMetadata? read = reads[i];
                if (read is null) continue;
                int? parentId = null;
                FileKey? parentKey = null;
                string? parentPath = Path.GetDirectoryName(read.Value.FullPath);
                if (parentPath is not null && idsByPath.TryGetValue(Path.GetFullPath(parentPath), out int parent))
                {
                    parentId = parent;
                    parentKey = records[parent - 1].Key;
                }
                int id = next++;
                FilenameRecord record = new(id, parentId, read.Value.Name, read.Value.FullPath,
                    read.Value.SizeBytes, read.Value.ModifiedUtc, read.Value.Flags)
                { Key = read.Value.Key, ParentKey = parentKey };
                records.Add(record);
                idsByPath[record.FullPath] = id;
            }
            batch.Clear();
        }
    }

    private IEnumerable<DiscoveredEntry> Discover(CancellationToken token)
    {
        var discovered = new ConcurrentBag<DiscoveredEntry>();
        DirectoryInfo rootInfo = new(root);
        discovered.Add(new DiscoveredEntry(root, rootInfo.Name, 0, rootInfo.LastWriteTimeUtc, true));
        var firstLevel = new List<string>();
        EnumerateChildren(root, firstLevel, discovered, token);
        Parallel.ForEach(firstLevel, new ParallelOptions
        {
            CancellationToken = token,
            MaxDegreeOfParallelism = Math.Max(4, Environment.ProcessorCount)
        }, first =>
        {
            var stack = new Stack<string>(); stack.Push(first);
            while (stack.Count > 0)
            {
                token.ThrowIfCancellationRequested();
                string dir = stack.Pop();
                var childDirectories = new List<string>();
                EnumerateChildren(dir, childDirectories, discovered, token);
                foreach (string child in childDirectories) stack.Push(child);
            }
        });
        foreach (DiscoveredEntry entry in discovered) yield return entry;

        void EnumerateChildren(string directory, ICollection<string> childDirectories,
            ConcurrentBag<DiscoveredEntry> output, CancellationToken cancellation)
        {
            IEnumerable<DiscoveredEntry?> children;
            try
            {
                var options = new EnumerationOptions
                {
                    IgnoreInaccessible = true, AttributesToSkip = FileAttributes.ReparsePoint,
                    ReturnSpecialDirectories = false
                };
                var fast = new FileSystemEnumerable<DiscoveredEntry?>(directory,
                    (ref FileSystemEntry entry) =>
                    {
                        try
                        {
                            string path = entry.ToFullPath();
                            if (IsExcluded(path)) return null;
                            bool dir = entry.IsDirectory;
                            ulong size = dir ? 0 : checked((ulong)entry.Length);
                            return new DiscoveredEntry(path, entry.FileName.ToString(), size,
                                entry.LastWriteTimeUtc.UtcDateTime, dir);
                        }
                        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
                        {
                            return null;
                        }
                    }, options);
                children = fast;
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { return; }
            foreach (DiscoveredEntry? entry in children)
            {
                cancellation.ThrowIfCancellationRequested();
                if (entry is not DiscoveredEntry value) continue;
                output.Add(value);
                if (value.IsDirectory) childDirectories.Add(value.FullPath);
            }
        }
    }

    /// <summary>Single-pass initial enumeration. Parent directories are yielded before their
    /// children, so the initial builder can assign parent identities without a second graph.
    /// </summary>
    private IEnumerable<DiscoveredEntry> DiscoverStreaming(CancellationToken token)
    {
        var stack = new Stack<string>();
        DirectoryInfo rootInfo = new(root);
        yield return new DiscoveredEntry(root, rootInfo.Name, 0, rootInfo.LastWriteTimeUtc, true);
        stack.Push(root);
        while (stack.Count > 0)
        {
            token.ThrowIfCancellationRequested();
            string directory = stack.Pop();
            IEnumerable<FileSystemInfo> children;
            try
            {
                children = new DirectoryInfo(directory).EnumerateFileSystemInfos("*", new EnumerationOptions
                {
                    IgnoreInaccessible = true, AttributesToSkip = FileAttributes.ReparsePoint,
                    ReturnSpecialDirectories = false
                });
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }

            var childDirectories = new List<string>();
            foreach (FileSystemInfo info in children)
            {
                token.ThrowIfCancellationRequested();
                string path = info.FullName;
                if (IsExcluded(path)) continue;
                FileAttributes attr;
                try { attr = info.Attributes; }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }
                if (attr.HasFlag(FileAttributes.ReparsePoint)) continue;
                bool isDirectory = attr.HasFlag(FileAttributes.Directory);
                ulong size = 0;
                DateTime modified;
                try
                {
                    size = isDirectory ? 0 : checked((ulong)((FileInfo)info).Length);
                    modified = info.LastWriteTimeUtc;
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }
                yield return new DiscoveredEntry(path, info.Name, size, modified, isDirectory);
                if (isDirectory) childDirectories.Add(path);
            }
            // LIFO traversal still yields a directory before all descendants. Sorting is not
            // required for FileIds because the parent map is keyed by exact path.
            for (int i = childDirectories.Count - 1; i >= 0; i--) stack.Push(childDirectories[i]);
        }
    }

    private FilenameRecord? TryReadRecord(DiscoveredEntry entry, int id)
    {
        try
        {
            return new FilenameRecord(id, null, entry.Name, entry.FullPath, entry.SizeBytes, entry.ModifiedUtc,
                entry.IsDirectory ? (byte)2 : (byte)1)
            { Key = VolumeIdentity.GetFileKey(entry.FullPath, entry.IsDirectory, volumeId) };
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { return null; }
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

    private ReadMetadata? TryReadMetadata(string path, bool isDirectory, FilenameRecord? prior = null)
    {
        try
        {
            FileSystemInfo info = isDirectory ? new DirectoryInfo(path) : new FileInfo(path);
            info.Refresh();
            ulong size = isDirectory ? 0 : checked((ulong)((FileInfo)info).Length);
            // A quiet restart needs only size/mtime/name for the common unchanged case.
            // Reuse the persisted native identity and avoid opening a second handle for
            // every entry; a key is fetched only for new or changed paths.
            FileKey key = prior is not null && prior.Name == info.Name && prior.FullPath == path &&
                prior.SizeBytes == size && prior.ModifiedUtc == info.LastWriteTimeUtc &&
                prior.Flags == (isDirectory ? (byte)2 : (byte)1)
                ? prior.Key
                : VolumeIdentity.GetFileKey(path, isDirectory, volumeId);
            return new ReadMetadata(path, info.Name, size, info.LastWriteTimeUtc,
                isDirectory ? (byte)2 : (byte)1, key);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { return null; }
    }

    private ReadMetadata? TryReadMetadata(DiscoveredEntry entry, FilenameRecord? prior)
    {
        try
        {
            byte flags = entry.IsDirectory ? (byte)2 : (byte)1;
            FileKey key = prior is not null && prior.Name == entry.Name && prior.FullPath == entry.FullPath &&
                prior.SizeBytes == entry.SizeBytes && prior.ModifiedUtc == entry.ModifiedUtc && prior.Flags == flags
                ? prior.Key
                : VolumeIdentity.GetFileKey(entry.FullPath, entry.IsDirectory, volumeId);
            return new ReadMetadata(entry.FullPath, entry.Name, entry.SizeBytes, entry.ModifiedUtc, flags, key);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { return null; }
    }

    private FilenameRecord AttachParent(FilenameRecord record)
    {
        string? parent = Path.GetDirectoryName(record.FullPath);
        return parent is not null && TryGetPathLocked(Path.GetFullPath(parent), out FilenameRecord? p)
            ? record with { ParentId = p!.FileId, ParentKey = p.Key } : record with { ParentId = null, ParentKey = null };
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
            ClearPathMapsLocked();
            int count = engine.BaseRecordCount;
            nextId = Math.Max(nextId, engine.MaxFileId == int.MaxValue ? int.MaxValue : engine.MaxFileId + 1);
            recordCount = count;
        }
        // Existing GUI startup can publish its first useful search while the persisted
        // Route C metadata is already available.  Starting the million-entry path hash
        // copy at the same instant contends with that cold first search and makes the
        // actual child-process startup sample sensitive to disk/JIT warm-up.  Give the
        // first result a short, deterministic head start; callers that need settled
        // filesystem state (WaitForIdleAsync) still await the same task and therefore
        // observe no semantic change in reconciliation or change-feed ordering.
        basePathIndexReady = Task.Run(async () =>
        {
            await Task.Delay(1500).ConfigureAwait(false);
            BuildLoadedBasePathIndex();
        });
    }

    private void BuildLoadedBasePathIndex()
    {
        int count = engine.BaseRecordCount;
        var hashes = new ulong[count];
        var ids = new int[count];
        int index = 0;
        engine.ForEachPathHash((id, hash) =>
        {
            if (index == hashes.Length)
            {
                Array.Resize(ref hashes, checked(index * 2 + 1));
                Array.Resize(ref ids, hashes.Length);
            }
            hashes[index] = hash; ids[index++] = id;
        });
        if (index != hashes.Length)
        {
            Array.Resize(ref hashes, index); Array.Resize(ref ids, index);
        }
        Array.Sort(hashes, ids);
        lock (gate)
        {
            if (disposed) return;
            basePathHashes = hashes;
            basePathIds = ids;
            recordCount = index;
        }
    }

    private bool TryGetPathLocked(string path, out FilenameRecord? record)
    {
        ulong hash = PathHash(path);
        if (pathIds.TryGetValue(hash, out int id))
        {
            if (id >= 0 && TryGetIdLocked(id, out record) && record!.FullPath.Equals(path, StringComparison.Ordinal)) return true;
            if (pathCollisions.TryGetValue(hash, out List<int>? ids))
                foreach (int candidate in ids)
                    if (TryGetIdLocked(candidate, out record) && record!.FullPath.Equals(path, StringComparison.Ordinal)) return true;
            record = null; return false;
        }
        if (TryGetBaseRange(hash, out int start, out int end))
        {
            for (int i = start; i < end; i++)
            {
                int candidate = basePathIds[i];
                if (TryGetIdLocked(candidate, out record) && record!.FullPath.Equals(path, StringComparison.Ordinal)) return true;
            }
        }
        record = null; return false;
    }

    private bool TryGetIdLocked(int id, out FilenameRecord? record)
    {
        if (mutableRecords.TryGetValue(id, out record)) return record is not null;
        return engine.TryGetRecord(id, out record);
    }

    private void AddPathLocked(FilenameRecord record)
    {
        AddPathHashLocked(record.FullPath, record.FileId);
    }

    private void AddPathHashLocked(string path, int fileId)
    {
        AddPathHashValueLocked(PathHash(path), fileId);
    }

    private void AddPathHashValueLocked(ulong hash, int fileId)
    {
        if (pathIds.TryGetValue(hash, out int existing))
        {
            if (existing == fileId) return;
            if (existing >= 0)
            {
                pathIds[hash] = -1;
                pathCollisions[hash] = [existing, fileId];
            }
            else if (pathCollisions.TryGetValue(hash, out List<int>? collisions)) collisions.Add(fileId);
            else pathIds[hash] = fileId;
            recordCount++;
            return;
        }
        if (!TryGetBaseRange(hash, out int start, out int end))
        {
            pathIds[hash] = fileId; recordCount++; return;
        }
        for (int i = start; i < end; i++) if (basePathIds[i] == fileId) return;
        var baseIds = new List<int>(end - start + 1);
        for (int i = start; i < end; i++) baseIds.Add(basePathIds[i]);
        baseIds.Add(fileId);
        pathIds[hash] = -1; pathCollisions[hash] = baseIds; recordCount++;
    }

    private void RemovePathLocked(string path, int fileId)
    {
        ulong hash = PathHash(path);
        if (pathCollisions.TryGetValue(hash, out List<int>? ids))
        {
            if (!ids.Remove(fileId)) return;
            if (ids.Count == 0)
            {
                if (TryGetBaseRange(hash, out _, out _)) { pathIds[hash] = -1; pathCollisions[hash] = []; }
                else { pathIds.Remove(hash); pathCollisions.Remove(hash); }
            }
            else if (ids.Count == 1 && !ContainsBaseId(hash, ids[0])) { pathIds[hash] = ids[0]; pathCollisions.Remove(hash); }
            else pathIds[hash] = -1;
            recordCount--;
        }
        else if (pathIds.TryGetValue(hash, out int current) && current == fileId)
        {
            if (ContainsBaseId(hash, fileId)) pathIds[hash] = -1;
            else pathIds.Remove(hash);
            if (recordCount > 0) recordCount--;
        }
        else if (!pathIds.ContainsKey(hash) && ContainsBaseId(hash, fileId))
        {
            pathIds[hash] = -1;
            if (recordCount > 0) recordCount--;
        }
    }

    private void ClearPathMapsLocked()
    {
        pathIds.Clear(); pathCollisions.Clear(); mutableRecords.Clear();
        basePathHashes = []; basePathIds = []; recordCount = 0;
    }

    private void BuildBasePathIndexLocked(IEnumerable<FilenameRecord> source, int count)
    {
        basePathHashes = new ulong[count]; basePathIds = new int[count];
        int index = 0;
        foreach (FilenameRecord record in source)
        {
            if (index == basePathHashes.Length)
            {
                Array.Resize(ref basePathHashes, checked(index * 2 + 1));
                Array.Resize(ref basePathIds, basePathHashes.Length);
            }
            basePathHashes[index] = PathHash(record.FullPath); basePathIds[index++] = record.FileId;
        }
        if (index != basePathHashes.Length)
        {
            Array.Resize(ref basePathHashes, index); Array.Resize(ref basePathIds, index);
        }
        Array.Sort(basePathHashes, basePathIds);
        recordCount = index;
    }

    private bool TryGetBaseRange(ulong hash, out int start, out int end)
    {
        int first = Array.BinarySearch(basePathHashes, hash);
        if (first < 0) { start = end = 0; return false; }
        start = first; while (start > 0 && basePathHashes[start - 1] == hash) start--;
        end = first + 1; while (end < basePathHashes.Length && basePathHashes[end] == hash) end++;
        return true;
    }

    private bool ContainsBaseId(ulong hash, int id)
    {
        return TryGetBaseRange(hash, out int start, out int end) &&
            Array.IndexOf(basePathIds, id, start, end - start) >= 0;
    }

    internal static ulong PathHash(string path)
    {
        unchecked
        {
            ulong hash = 14695981039346656037UL;
            foreach (char c in path) { hash ^= c; hash *= 1099511628211UL; }
            return hash;
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
        try { await basePathIndexReady.ConfigureAwait(false); } catch (OperationCanceledException) { }
        persistence.MarkClean(); persistence.Dispose();
        engineGate.EnterWriteLock(); try { engine.Dispose(); } finally { engineGate.ExitWriteLock(); engineGate.Dispose(); }
        lock (gate) ClearPathMapsLocked();
        stop.Dispose();
    }
}

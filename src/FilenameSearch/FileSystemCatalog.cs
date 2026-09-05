using System.Collections.Concurrent;
using System.Threading.Channels;

namespace PersonalRag.FilenameSearch;

/// <summary>Filesystem event kind kept outside the search engine.</summary>
internal enum FileSystemEventKind { Created, Changed, Deleted, Renamed }

internal sealed record FileSystemEvent(string Path, string? OldPath = null, FileSystemEventKind Kind = FileSystemEventKind.Changed, bool Reconcile = false, bool CatchUp = false);

/// <summary>
/// Collects local files and directories and keeps the selected filename engine current.
/// Enumeration is deliberately conservative: inaccessible and reparse-point entries are
/// skipped, while one bad entry never aborts the rest of a scan.
/// </summary>
public sealed class FileSystemCatalog : IAsyncDisposable
{
    private readonly object gate = new();
    private readonly ReaderWriterLockSlim engineGate = new(LockRecursionPolicy.NoRecursion);
    private readonly string root;
    private readonly string store;
    private readonly string storeDirectory;
    private FilenameSearchEngine engine;
    private readonly Dictionary<string, FilenameRecord> byPath = new(StringComparer.OrdinalIgnoreCase);
    private readonly Channel<FileSystemEvent> events = Channel.CreateUnbounded<FileSystemEvent>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });
    private readonly Channel<bool> persistRequests = Channel.CreateUnbounded<bool>(
        new UnboundedChannelOptions { SingleReader = true, SingleWriter = false });
    private readonly CancellationTokenSource stop = new();
    private readonly Task worker;
    private readonly Task persister;
    private readonly FileSystemWatcher watcher;
    private readonly TaskCompletionSource ready = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private int nextId = 1;
    private int forceReconcile;
    private int pendingEvents;
    private string status = "準備中";
    private bool started;
    private bool disposed;

    private FileSystemCatalog(string root, string store, FilenameSearchEngine engine)
    {
        this.root = NormalizePath(root);
        this.store = Path.GetFullPath(store);
        storeDirectory = Path.GetDirectoryName(this.store) ?? throw new ArgumentException("Store path has no directory", nameof(store));
        this.engine = engine;
        watcher = new FileSystemWatcher(this.root)
        {
            IncludeSubdirectories = true,
            InternalBufferSize = 64 * 1024,
            NotifyFilter = NotifyFilters.FileName | NotifyFilters.DirectoryName | NotifyFilters.LastWrite | NotifyFilters.Size,
            Filter = "*"
        };
        watcher.Created += (_, args) => Queue(new FileSystemEvent(args.FullPath, Kind: FileSystemEventKind.Created, Reconcile: Directory.Exists(args.FullPath)));
        watcher.Changed += (_, args) => Queue(new FileSystemEvent(args.FullPath, Kind: FileSystemEventKind.Changed));
        watcher.Deleted += (_, args) => Queue(new FileSystemEvent(args.FullPath, Kind: FileSystemEventKind.Deleted));
        watcher.Renamed += (_, args) => Queue(new FileSystemEvent(args.FullPath, args.OldFullPath, FileSystemEventKind.Renamed, Directory.Exists(args.FullPath)));
        watcher.Error += (_, _) =>
        {
            Interlocked.Exchange(ref forceReconcile, 1);
            Queue(new FileSystemEvent(this.root, Reconcile: true));
        };
        worker = Task.Run(UpdateLoop);
        persister = Task.Run(PersistLoop);
    }

    public string Root => root;
    public string Store => store;
    public string Status => Volatile.Read(ref status);
    public bool IsReady => ready.Task.IsCompletedSuccessfully;
    public Task Ready => ready.Task;
    public event Action? Changed;

    /// <summary>Opens an existing store immediately, then catches up with the filesystem.</summary>
    public static FileSystemCatalog Open(string root, string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(root);
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string fullRoot = NormalizePath(root);
        if (!Directory.Exists(fullRoot)) throw new DirectoryNotFoundException(fullRoot);
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(store))!);
        var engine = new FilenameSearchEngine();
        var catalog = new FileSystemCatalog(fullRoot, store, engine);
        bool loaded = false;
        try
        {
            if (File.Exists(catalog.store))
            {
                engine.Load(catalog.store);
                catalog.LoadExistingRecords();
                engine.WarmUp();
                loaded = catalog.byPath.Keys.Any(path => catalog.IsWithinRoot(path));
            }
        }
        catch (Exception ex) when (ex is IOException or InvalidDataException or UnauthorizedAccessException)
        {
            catalog.SetStatus("保存済みインデックスを検証できません: " + ex.Message);
            loaded = false;
        }

        if (loaded)
        {
            catalog.SetStatus("差分を確認中");
            catalog.ready.TrySetResult();
        }
        else
        {
            catalog.RebuildCore();
            catalog.SetStatus("Ready");
            catalog.ready.TrySetResult();
            catalog.PersistSoon();
        }
        catalog.Start();
        return catalog;
    }

    public IReadOnlyList<FilenameRecord> Records
    {
        get
        {
            lock (gate) return byPath.Values.OrderBy(record => record.FileId).ToArray();
        }
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ThrowIfDisposed();
        engineGate.EnterReadLock();
        try { return engine.Search(request); }
        finally { engineGate.ExitReadLock(); }
    }

    /// <summary>Performs a complete safe catch-up and persists the resulting snapshot.</summary>
    public Task ReconcileAsync()
    {
        ThrowIfDisposed();
        return Task.Run(() => RebuildCore(), stop.Token);
    }

    public async Task WaitForIdleAsync(TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        if (timeout <= TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(timeout));
        using var timeoutCts = CancellationTokenSource.CreateLinkedTokenSource(stop.Token, cancellationToken);
        timeoutCts.CancelAfter(timeout);
        while (!timeoutCts.IsCancellationRequested)
        {
            if (Status == "Ready" && Volatile.Read(ref pendingEvents) == 0) return;
            await Task.Delay(20, timeoutCts.Token).ConfigureAwait(false);
        }
        timeoutCts.Token.ThrowIfCancellationRequested();
    }

    private void Start()
    {
        if (started) return;
        started = true;
        try { watcher.EnableRaisingEvents = true; }
        catch
        {
            watcher.Dispose();
            throw;
        }
        Queue(new FileSystemEvent(root, CatchUp: true));
    }

    private void Queue(FileSystemEvent change)
    {
        if (disposed || IsExcluded(change.Path)) return;
        if (change.Reconcile) Interlocked.Exchange(ref forceReconcile, 1);
        if (events.Writer.TryWrite(change)) Interlocked.Increment(ref pendingEvents);
        else Interlocked.Exchange(ref forceReconcile, 1);
    }

    private async Task UpdateLoop()
    {
        try
        {
            while (await events.Reader.WaitToReadAsync(stop.Token).ConfigureAwait(false))
            {
                await Task.Delay(50, stop.Token).ConfigureAwait(false);
                var pending = new List<FileSystemEvent>();
                while (events.Reader.TryRead(out FileSystemEvent? change))
                {
                    Interlocked.Decrement(ref pendingEvents);
                    pending.Add(change);
                }
                bool rebuild = Interlocked.Exchange(ref forceReconcile, 0) != 0 || pending.Any(change => change.Reconcile);
                try
                {
                    SetStatus("変更を反映中");
                    bool changed = pending.Any(change => change.CatchUp) && !rebuild
                        ? CatchUpCore()
                        : rebuild ? RebuildCore() : ProcessEvents(pending);
                    SetStatus("Ready");
                    if (changed) PersistSoon();
                }
                catch (Exception ex) when (ex is IOException or InvalidDataException or UnauthorizedAccessException)
                {
                    Interlocked.Exchange(ref forceReconcile, 1);
                    SetStatus("変更反映エラー: " + ex.Message);
                    Queue(new FileSystemEvent(root, Reconcile: true));
                }
            }
        }
        catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
    }

    private bool ProcessEvents(IReadOnlyList<FileSystemEvent> pending)
    {
        engineGate.EnterReadLock();
        try
        {
        // Deduplicate by path while preserving a rename pair. FileSystemWatcher can raise
        // several Change events for one write.
        var unique = new Dictionary<string, FileSystemEvent>(StringComparer.OrdinalIgnoreCase);
        bool changed = false;
        foreach (FileSystemEvent change in pending)
        {
            if (change.OldPath is not null) changed |= ProcessRename(change.OldPath, change.Path);
            else unique[NormalizePath(change.Path)] = change;
        }
        foreach (FileSystemEvent change in unique.Values) changed |= ProcessPath(change.Path, change.Kind);
        return changed;
        }
        finally { engineGate.ExitReadLock(); }
    }

    private bool ProcessRename(string oldPath, string newPath)
    {
        oldPath = NormalizePath(oldPath);
        newPath = NormalizePath(newPath);
        if (IsExcluded(oldPath) && IsExcluded(newPath)) return false;
        lock (gate)
        {
            if (!byPath.TryGetValue(oldPath, out FilenameRecord? oldRecord))
            {
                Interlocked.Exchange(ref forceReconcile, 1);
                return false;
            }
            byPath.Remove(oldPath);
            engine.Remove(oldRecord.FileId);
            if (Directory.Exists(newPath))
            {
                Interlocked.Exchange(ref forceReconcile, 1);
                return true;
            }
            if (File.Exists(newPath))
            {
                FilenameRecord replacement = ReadRecord(newPath, oldRecord.FileId);
                byPath[newPath] = replacement;
                engine.Upsert(replacement);
            }
            return true;
        }
    }

    private bool ProcessPath(string path, FileSystemEventKind kind)
    {
        path = NormalizePath(path);
        if (IsExcluded(path)) return false;
        if (Directory.Exists(path))
        {
            if (kind == FileSystemEventKind.Changed) return false;
            Interlocked.Exchange(ref forceReconcile, 1);
            return false;
        }
        lock (gate)
        {
            if (File.Exists(path))
            {
                int id = byPath.TryGetValue(path, out FilenameRecord? current) ? current.FileId : AllocateId();
                FilenameRecord next = ReadRecord(path, id);
                if (current is not null && Equivalent(current, next)) return false;
                byPath[path] = next;
                engine.Upsert(next);
                return true;
            }
            else if (byPath.Remove(path, out FilenameRecord? removed))
            {
                engine.Remove(removed.FileId);
                if (removed.IsDirectory) Interlocked.Exchange(ref forceReconcile, 1);
                return true;
            }
            return false;
        }
    }

    private bool RebuildCore()
    {
        FileSystemEntry[] discovered = Discover();
        Dictionary<string, FilenameRecord> next;
        lock (gate)
        {
            var prior = new Dictionary<string, FilenameRecord>(byPath, StringComparer.OrdinalIgnoreCase);
            var reserved = prior.Values.Select(record => record.FileId).ToHashSet();
            var assigned = new HashSet<int>();
            var movedCandidates = prior.Values
                .GroupBy(Signature)
                .ToDictionary(group => group.Key, group => new Queue<FilenameRecord>(group), EqualityComparer<FileSignature>.Default);
            var idsByPath = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
            var preliminary = new List<(string Path, bool Directory, int Id, FilenameRecord Metadata)>();
            foreach (FileSystemEntry entry in discovered)
            {
                FilenameRecord metadata = ReadRecord(entry.Path, 0);
                int id;
                if (prior.TryGetValue(entry.Path, out FilenameRecord? old))
                {
                    id = old.FileId;
                }
                else if (movedCandidates.TryGetValue(Signature(metadata), out Queue<FilenameRecord>? candidates))
                {
                    while (candidates.Count > 0 && assigned.Contains(candidates.Peek().FileId)) candidates.Dequeue();
                    id = candidates.Count > 0 ? candidates.Dequeue().FileId : AllocateId(reserved);
                }
                else
                {
                    id = AllocateId(reserved);
                }
                assigned.Add(id);
                reserved.Add(id);
                idsByPath[entry.Path] = id;
                preliminary.Add((entry.Path, entry.IsDirectory, id, metadata));
            }
            next = new Dictionary<string, FilenameRecord>(StringComparer.OrdinalIgnoreCase);
            foreach ((string path, bool isDirectory, int id, FilenameRecord metadata) in preliminary)
            {
                FilenameRecord record = metadata with { FileId = id };
                string? parentPath = Path.GetDirectoryName(path);
                int? parentId = parentPath is not null && idsByPath.TryGetValue(NormalizePath(parentPath), out int parent) ? parent : null;
                next[path] = record with { ParentId = parentId, Flags = isDirectory ? (byte)2 : (byte)1 };
            }
        }

        // Build the replacement off the live engine. Existing-index searches remain usable
        // while a startup catch-up scans and indexes a large tree.
        var rebuilt = new FilenameSearchEngine();
        rebuilt.Build(next.Values.OrderBy(record => record.FileId).ToArray());
        FilenameSearchEngine previous;
        engineGate.EnterWriteLock();
        try
        {
            lock (gate)
            {
                previous = engine;
                engine = rebuilt;
                byPath.Clear();
                foreach ((string path, FilenameRecord record) in next) byPath[path] = record;
            }
        }
        finally { engineGate.ExitWriteLock(); }
        previous.Dispose();
        return true;
    }

    private bool CatchUpCore()
    {
        FileSystemEntry[] discovered = Discover();
        Dictionary<string, FilenameRecord> prior;
        lock (gate) prior = new Dictionary<string, FilenameRecord>(byPath, StringComparer.OrdinalIgnoreCase);
        var reusable = new bool[discovered.Length];
        Parallel.For(0, discovered.Length, index =>
        {
            FileSystemEntry entry = discovered[index];
            if (!prior.TryGetValue(entry.Path, out FilenameRecord? old)) return;
            if (old.IsDirectory)
            {
                reusable[index] = true;
                return;
            }
            try { reusable[index] = File.GetLastWriteTimeUtc(entry.Path) == old.ModifiedUtc; }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { }
        });
        lock (gate)
        {
            var discoveredPaths = discovered.Select(entry => entry.Path).ToHashSet(StringComparer.OrdinalIgnoreCase);
            var reserved = prior.Values.Select(record => record.FileId).ToHashSet();
            var assigned = new HashSet<int>();
            var movedCandidates = prior.Values
                .Where(record => !discoveredPaths.Contains(record.FullPath))
                .GroupBy(Signature)
                .ToDictionary(group => group.Key, group => new Queue<FilenameRecord>(group), EqualityComparer<FileSignature>.Default);
            var idsByPath = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
            var preliminary = new List<(string Path, bool Directory, int Id, FilenameRecord Metadata)>();
            for (int discoveredIndex = 0; discoveredIndex < discovered.Length; discoveredIndex++)
            {
                FileSystemEntry entry = discovered[discoveredIndex];
                FilenameRecord metadata;
                int id;
                if (prior.TryGetValue(entry.Path, out FilenameRecord? old))
                {
                    id = old.FileId;
                    // A restart normally has an unchanged tree. Reuse the persisted metadata
                    // after one cheap timestamp check; the previous full FileInfo.Refresh per
                    // entry made a 100k-file catch-up exceed the five-second product gate.
                    if (reusable[discoveredIndex])
                        metadata = old;
                    else metadata = ReadRecord(entry.Path, id);
                }
                else
                {
                    metadata = ReadRecord(entry.Path, 0);
                    if (movedCandidates.TryGetValue(Signature(metadata), out Queue<FilenameRecord>? candidates))
                    {
                        while (candidates.Count > 0 && assigned.Contains(candidates.Peek().FileId)) candidates.Dequeue();
                        id = candidates.Count > 0 ? candidates.Dequeue().FileId : AllocateId(reserved);
                    }
                    else id = AllocateId(reserved);
                }
                assigned.Add(id);
                reserved.Add(id);
                idsByPath[entry.Path] = id;
                preliminary.Add((entry.Path, entry.IsDirectory, id, metadata));
            }
            var next = new Dictionary<string, FilenameRecord>(StringComparer.OrdinalIgnoreCase);
            foreach ((string path, bool isDirectory, int id, FilenameRecord metadata) in preliminary)
            {
                string? parentPath = Path.GetDirectoryName(path);
                int? parentId = parentPath is not null && idsByPath.TryGetValue(NormalizePath(parentPath), out int parent) ? parent : null;
                next[path] = metadata with { FileId = id, ParentId = parentId, Flags = isDirectory ? (byte)2 : (byte)1 };
            }

            var nextIds = next.Values.Select(record => record.FileId).ToHashSet();
            int changedCount = prior.Values.Count(record => !nextIds.Contains(record.FileId));
            changedCount += next.Count(pair => !prior.TryGetValue(pair.Key, out FilenameRecord? old) || !Equivalent(old, pair.Value));
            if (changedCount > 512)
            {
                // A large restart delta would force Route C's overlay into a full scan for
                // every query. Build a replacement off the live engine so the GUI can keep
                // serving the committed snapshot while catch-up indexes in the background.
                var rebuilt = new FilenameSearchEngine();
                rebuilt.Build(next.Values.OrderBy(record => record.FileId).ToArray());
                FilenameSearchEngine previous;
                engineGate.EnterWriteLock();
                try
                {
                    previous = engine;
                    engine = rebuilt;
                    byPath.Clear();
                    foreach ((string path, FilenameRecord record) in next) byPath[path] = record;
                }
                finally { engineGate.ExitWriteLock(); }
                previous.Dispose();
                return true;
            }

            engineGate.EnterReadLock();
            try
            {
                var nextIdsForApply = next.Values.Select(record => record.FileId).ToHashSet();
                foreach (FilenameRecord old in prior.Values)
                    if (!nextIdsForApply.Contains(old.FileId)) engine.Remove(old.FileId);
                foreach ((string path, FilenameRecord current) in next)
                    if (!prior.TryGetValue(path, out FilenameRecord? old) || !Equivalent(old, current)) engine.Upsert(current);
            }
            finally { engineGate.ExitReadLock(); }
            byPath.Clear();
            foreach ((string path, FilenameRecord record) in next) byPath[path] = record;
            return changedCount > 0;
        }
    }

    private static bool Equivalent(FilenameRecord left, FilenameRecord right) =>
        left.FileId == right.FileId && left.ParentId == right.ParentId &&
        left.Name == right.Name && left.FullPath == right.FullPath &&
        left.SizeBytes == right.SizeBytes && left.ModifiedUtc == right.ModifiedUtc && left.Flags == right.Flags;

    private void LoadExistingRecords()
    {
        lock (gate)
        {
            byPath.Clear();
            foreach (FilenameRecord record in engine.Records)
            {
                string path = NormalizePath(record.FullPath);
                if (!IsWithinRoot(path) || IsExcluded(path)) continue;
                byPath[path] = record with { FullPath = path, Name = Path.GetFileName(path) };
                if (record.FileId >= nextId) nextId = record.FileId == int.MaxValue ? int.MaxValue : record.FileId + 1;
            }
        }
    }

    private FileSystemEntry[] Discover()
    {
        var entries = new List<FileSystemEntry>();
        var pending = new Stack<string>();
        pending.Push(root);
        entries.Add(new FileSystemEntry(root, true));
        while (pending.Count > 0)
        {
            string directory = pending.Pop();
            IEnumerable<string> children;
            try
            {
                children = Directory.EnumerateFileSystemEntries(directory, "*", new EnumerationOptions
                {
                    IgnoreInaccessible = true,
                    RecurseSubdirectories = false,
                    ReturnSpecialDirectories = false,
                    AttributesToSkip = FileAttributes.ReparsePoint
                }).ToArray();
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                continue;
            }
            foreach (string rawPath in children)
            {
                string path = NormalizePath(rawPath);
                if (IsExcluded(path)) continue;
                FileAttributes attributes;
                try { attributes = File.GetAttributes(path); }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { continue; }
                if (attributes.HasFlag(FileAttributes.ReparsePoint)) continue;
                bool isDirectory = attributes.HasFlag(FileAttributes.Directory);
                entries.Add(new FileSystemEntry(path, isDirectory));
                if (isDirectory) pending.Push(path);
            }
        }
        return entries.OrderBy(entry => entry.Path, StringComparer.OrdinalIgnoreCase).ToArray();
    }

    private FilenameRecord ReadRecord(string path, int id)
    {
        path = NormalizePath(path);
        bool isDirectory = Directory.Exists(path);
        try
        {
            FileSystemInfo info = isDirectory ? new DirectoryInfo(path) : new FileInfo(path);
            info.Refresh();
            ulong size = isDirectory ? 0UL : checked((ulong)((FileInfo)info).Length);
            string name = isDirectory ? ((DirectoryInfo)info).Name : ((FileInfo)info).Name;
            return new FilenameRecord(id, null, name.Normalize(), path.Normalize(), size, info.LastWriteTimeUtc, isDirectory ? (byte)2 : (byte)1);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The event raced with deletion or an ACL change. A zero-sized metadata record
            // is safe to replace on the next watcher/reconcile pass.
            return new FilenameRecord(id, null, Path.GetFileName(path), path, 0, DateTime.UtcNow, isDirectory ? (byte)2 : (byte)1);
        }
    }

    private int AllocateId() => AllocateId(null);

    private int AllocateId(HashSet<int>? used)
    {
        while (nextId <= 0 || (used is not null && used.Contains(nextId)))
        {
            if (nextId == int.MaxValue) throw new InvalidOperationException("File ID space exhausted");
            nextId++;
        }
        return nextId++;
    }

    private static FileSignature Signature(FilenameRecord record) =>
        new(record.Name, record.SizeBytes, record.ModifiedUtc.Ticks, record.Flags);

    private bool IsWithinRoot(string path) =>
        path.Equals(root, StringComparison.OrdinalIgnoreCase) ||
        path.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase);

    private bool IsExcluded(string path)
    {
        path = NormalizePath(path);
        string excluded = store;
        bool storeDirectoryIsChild = !storeDirectory.Equals(root, StringComparison.OrdinalIgnoreCase) && IsWithinRoot(storeDirectory);
        return path.Equals(excluded, StringComparison.OrdinalIgnoreCase) ||
            path.StartsWith(excluded + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase) ||
            storeDirectoryIsChild && (path.Equals(storeDirectory, StringComparison.OrdinalIgnoreCase) ||
            path.StartsWith(storeDirectory + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase));
    }

    private void PersistSoon()
    {
        if (!disposed) persistRequests.Writer.TryWrite(true);
    }

    private async Task PersistLoop()
    {
        try
        {
            while (await persistRequests.Reader.WaitToReadAsync(stop.Token).ConfigureAwait(false))
            {
                await Task.Delay(250, stop.Token).ConfigureAwait(false);
                while (persistRequests.Reader.TryRead(out _)) { }
                try
                {
                    SetStatus("保存中");
                    FilenameRecord[] snapshot;
                    lock (gate) snapshot = byPath.Values.OrderBy(record => record.FileId).ToArray();
                    FilenameSearchEngine.SaveSnapshotAtomic(store, snapshot);
                    SetStatus("Ready");
                    RaiseChanged();
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException)
                {
                    SetStatus("保存エラー: " + ex.Message);
                    RaiseChanged();
                }
            }
        }
        catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
    }

    private void SetStatus(string value)
    {
        Volatile.Write(ref status, value);
        RaiseChanged();
    }

    private void RaiseChanged()
    {
        try { Changed?.Invoke(); }
        catch { /* observers cannot stop the indexing worker */ }
    }

    public async ValueTask DisposeAsync()
    {
        if (disposed) return;
        disposed = true;
        watcher.EnableRaisingEvents = false;
        watcher.Dispose();
        stop.Cancel();
        events.Writer.TryComplete();
        persistRequests.Writer.TryComplete();
        try { await worker.ConfigureAwait(false); } catch (OperationCanceledException) { }
        try { await persister.ConfigureAwait(false); } catch (OperationCanceledException) { }
        // Persist the last in-memory snapshot even when shutdown races a debounce timer.
        engineGate.EnterWriteLock();
        try
        {
            try { engine.SaveAtomic(store); }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException)
            { SetStatus("終了時保存エラー: " + ex.Message); }
            engine.Dispose();
        }
        finally
        {
            engineGate.ExitWriteLock();
            engineGate.Dispose();
        }
        stop.Dispose();
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);
    private static string NormalizePath(string path) => Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
    private readonly record struct FileSystemEntry(string Path, bool IsDirectory);
    private readonly record struct FileSignature(string Name, ulong SizeBytes, long ModifiedUtcTicks, byte Flags);
}

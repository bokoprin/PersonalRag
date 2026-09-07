using System.Diagnostics;
using System.Buffers;
using System.Buffers.Binary;
using System.Text;
using CoreFileRecord = FilenameSearch.Core.FileRecord;
using CoreQuery = FilenameSearch.Core.FilenameQuery;
using CoreScope = FilenameSearch.Core.FilenameScope;
using CoreFilenameSemantics = FilenameSearch.Core.FilenameSemantics;
using RouteCEngine = FilenameSearch.RouteC.RouteCEngine;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Production adapter around the frozen Route C immutable base. Live changes are held in a
/// separately indexed delta plus tombstones; they never call RouteCEngine.Upsert, because the
/// bake-off Route C intentionally falls back to a full scan whenever its own overlay is nonempty.
/// </summary>
public sealed class FilenameSearchEngine : IFilenameSearch
{
    internal sealed class StableMetadataSnapshot
    {
        private readonly ulong[] sizes;
        private readonly long[] modifiedTicks;
        private readonly byte[] flags;
        private readonly bool[] present;

        internal int Capacity => present.Length;
        internal int LiveCount { get; }

        internal StableMetadataSnapshot(ulong[] sizes, long[] modifiedTicks, byte[] flags,
            bool[] present, int liveCount)
        {
            this.sizes = sizes;
            this.modifiedTicks = modifiedTicks;
            this.flags = flags;
            this.present = present;
            LiveCount = liveCount;
        }

        internal bool TryGet(int id, out ulong size, out long ticks, out byte entryFlags)
        {
            if ((uint)id >= (uint)present.Length || !present[id])
            {
                size = 0; ticks = 0; entryFlags = 0;
                return false;
            }
            size = sizes[id]; ticks = modifiedTicks[id]; entryFlags = flags[id];
            return true;
        }
    }

    private readonly object gate = new();
    private RouteCEngine baseEngine = new();
    private ExactTable? baseTable;
    private int baseCount;
    private readonly Dictionary<int, FilenameRecord?> delta = [];
    private readonly DeltaIndex deltaIndex = new();
    private bool disposed;

    public IReadOnlyList<FilenameRecord> Records
    {
        get
        {
            lock (gate)
            {
                ThrowIfDisposed();
                return SnapshotRecords();
            }
        }
    }

    public int OverlayCount { get { lock (gate) return delta.Count; } }
    internal int BaseRecordCount { get { lock (gate) return baseCount; } }

    internal Dictionary<string, FilenameRecord> SnapshotByPath()
    {
        lock (gate)
        {
            ThrowIfDisposed();
            var result = new Dictionary<string, FilenameRecord>(baseCount + delta.Count, StringComparer.Ordinal);
            HashSet<int>? removedIds = delta.Count == 0
                ? null
                : delta.Where(pair => pair.Value is null).Select(pair => pair.Key).ToHashSet();
            if (baseTable is not null)
                foreach (FilenameRecord record in baseTable.Records())
                {
                    if (removedIds is null || !removedIds.Contains(record.FileId))
                        result[record.FullPath] = record;
                }
            foreach ((int id, FilenameRecord? record) in delta)
            {
                if (record is not null) result[record.FullPath] = record;
            }
            return result;
        }
    }

    public bool ShouldCompact
    {
        get
        {
            lock (gate)
            {
                int threshold = Math.Max(4_096, Math.Max(1, baseCount / 100));
                return delta.Count >= threshold;
            }
        }
    }

    public void Build(IReadOnlyList<FilenameRecord> records)
    {
        ArgumentNullException.ThrowIfNull(records);
        // The production catalog assigns monotonically increasing FileIds. Reusing that
        // order avoids a second million-record reference array during the initial build;
        // standalone callers that provide an unsorted list still receive the historical
        // deterministic ordering.
        IReadOnlyList<FilenameRecord> exact = IsSortedByFileId(records)
            ? records
            : records.OrderBy(r => r.FileId).ToArray();
        ValidateExact(exact);
        var next = new RouteCEngine();
        int[] ids = new int[exact.Count];
        var normalizedNames = new string[exact.Count];
        var normalizedPaths = new string[exact.Count];
        for (int i = 0; i < exact.Count; i++)
        {
            ids[i] = exact[i].FileId;
            normalizedNames[i] = CoreFilenameSemantics.Normalize(exact[i].Name, false);
            normalizedPaths[i] = CoreFilenameSemantics.Normalize(exact[i].FullPath, false);
        }
        next.Build(ids, normalizedNames, normalizedPaths);
        normalizedNames = null!;
        normalizedPaths = null!;
        lock (gate)
        {
            ThrowIfDisposed();
            baseEngine.Dispose();
            baseEngine = next;
            baseTable?.Dispose();
            baseTable = ExactTable.Create(exact, out baseCount);
            delta.Clear();
            deltaIndex.Clear();
        }
    }

    public void LoadBase(string indexPath, IReadOnlyList<FilenameRecord> exactRecords)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(indexPath);
        ArgumentNullException.ThrowIfNull(exactRecords);
        IReadOnlyList<FilenameRecord> exact = IsSortedByFileId(exactRecords)
            ? exactRecords
            : exactRecords.OrderBy(r => r.FileId).ToArray();
        ValidateExact(exact);
        var next = new RouteCEngine();
        next.Load(Path.GetFullPath(indexPath));
        int[] routeIds = next.FileIds.ToArray();
        int[] exactIds = exact.Select(r => r.FileId).OrderBy(x => x).ToArray();
        if (!routeIds.AsSpan().SequenceEqual(exactIds))
        {
            next.Dispose();
            throw new InvalidDataException("Route C base/index metadata identity mismatch");
        }
        lock (gate)
        {
            ThrowIfDisposed();
            baseEngine.Dispose();
            baseEngine = next;
            baseTable?.Dispose();
            baseTable = ExactTable.Create(exact, out baseCount);
            delta.Clear();
            deltaIndex.Clear();
        }
    }

    public void SaveBase(string indexPath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(indexPath);
        lock (gate)
        {
            ThrowIfDisposed();
            if (delta.Count != 0)
                throw new InvalidOperationException("SaveBase requires a compacted engine with an empty delta");
            baseEngine.Save(Path.GetFullPath(indexPath));
        }
    }

    /// <summary>
    /// Compatibility helper for tests/tools that need one compacted snapshot.
    /// Product persistence uses GenerationStore instead.
    /// </summary>
    public void SaveAtomic(string indexPath)
    {
        string full = Path.GetFullPath(indexPath);
        string? parent = Path.GetDirectoryName(full);
        if (parent is null) throw new ArgumentException("Index path has no parent", nameof(indexPath));
        Directory.CreateDirectory(parent);
        string temp = full + "." + Guid.NewGuid().ToString("N") + ".tmp";
        using var compacted = CreateCompacted();
        try
        {
            compacted.SaveBase(temp);
            if (File.Exists(full)) File.Replace(temp, full, null, true);
            else File.Move(temp, full);
        }
        finally
        {
            try { if (File.Exists(temp)) File.Delete(temp); } catch { }
        }
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        request.Validate();
        var watch = Stopwatch.StartNew();
        lock (gate)
        {
            ThrowIfDisposed();
            bool hasWildcard = HasWildcard(request.Query);
            string broadQuery = hasWildcard ? BuildBroadLiteralQuery(request.Query) : request.Query;

            // Route C is a conservative candidate generator. A bounded candidate prefix is
            // enough when it already yields the requested number of exact matches (the
            // common query path); if false positives consume that prefix, retry the complete
            // candidate set below so Limit never introduces a false negative.
            int routeLimit = request.Limit > 0
                ? request.Limit > 4_096
                    ? request.Limit
                    : Math.Min(checked(request.Limit * 4), 4_096)
                : 0;
            CoreQuery routeQuery = new(
                broadQuery,
                request.Scope == SearchScope.Filename ? CoreScope.Filename : CoreScope.FullPath,
                request.CaseSensitive,
                routeLimit);
            var route = baseEngine.Search(routeQuery);
            var merged = new Dictionary<int, FilenameRecord>();

            void AddBaseCandidates(IReadOnlyList<CoreFileRecord> candidates)
            {
                foreach (CoreFileRecord hit in candidates)
                {
                    if (delta.ContainsKey(hit.FileId)) continue;
                    if (!TryGetBase(hit.FileId, out FilenameRecord? exact) || exact is null) continue;
                    // Route C is intentionally a conservative candidate engine. Search
                    // semantics are owned here so exact path spelling, full Unicode case
                    // folding, and substring wildcard behavior have one authority.
                    if (!Matches(exact, request)) continue;
                    merged[exact.FileId] = exact;
                    if (request.Limit > 0 && merged.Count >= request.Limit) break;
                }
            }

            AddBaseCandidates(route.Records);
            if (request.Limit > 0 && merged.Count < request.Limit &&
                route.Candidates > route.Records.Count)
            {
                // The prefix did not contain enough true matches. Re-run without a
                // candidate limit before merging the indexed delta, preserving complete
                // results for every query shape.
                route = baseEngine.Search(routeQuery with { Limit = 0 });
                merged.Clear();
                AddBaseCandidates(route.Records);
            }

            IReadOnlyList<FilenameRecord> deltaMatches = deltaIndex.Search(
                request,
                delta.Where(pair => pair.Value is not null)
                    .ToDictionary(pair => pair.Key, pair => pair.Value!));
            foreach (FilenameRecord hit in deltaMatches) merged[hit.FileId] = hit;

            FilenameRecord[] ordered = merged.Values.OrderBy(r => r.FileId).ToArray();
            if (request.Limit > 0 && ordered.Length > request.Limit)
                ordered = ordered[..request.Limit];

            watch.Stop();
            return new FilenameSearchResult(
                ordered,
                watch.Elapsed.TotalMilliseconds,
                route.Candidates + deltaIndex.LastCandidateCount,
                route.UsedScan || deltaIndex.LastUsedScan,
                request.RequestId);
        }
    }

    public void Upsert(FilenameRecord record)
    {
        ArgumentNullException.ThrowIfNull(record);
        lock (gate)
        {
            ThrowIfDisposed();
            delta[record.FileId] = record;
            deltaIndex.Upsert(record);
        }
    }

    public bool Remove(int fileId)
    {
        lock (gate)
        {
            ThrowIfDisposed();
            bool existed = TryGetBase(fileId, out _) ||
                (delta.TryGetValue(fileId, out FilenameRecord? current) && current is not null);
            if (!existed) return false;
            delta[fileId] = null;
            deltaIndex.Remove(fileId);
            return true;
        }
    }

    public FilenameSearchEngine CreateCompacted()
    {
        FilenameRecord[] snapshot;
        lock (gate)
        {
            ThrowIfDisposed();
            snapshot = SnapshotRecords();
        }
        var compacted = new FilenameSearchEngine();
        compacted.Build(snapshot);
        return compacted;
    }

    public void Dispose()
    {
        lock (gate)
        {
            if (disposed) return;
            baseEngine.Dispose();
            baseTable?.Dispose();
            baseTable = null;
            baseCount = 0;
            delta.Clear();
            deltaIndex.Clear();
            disposed = true;
        }
        GC.SuppressFinalize(this);
    }

    private FilenameRecord[] SnapshotRecords()
    {
        var result = new Dictionary<int, FilenameRecord>(baseCount + delta.Count);
        if (baseTable is not null)
            foreach (FilenameRecord record in baseTable.Records()) result[record.FileId] = record;
        foreach ((int id, FilenameRecord? record) in delta)
        {
            if (record is null) result.Remove(id);
            else result[id] = record;
        }
        return result.Values.OrderBy(r => r.FileId).ToArray();
    }

    private static CoreFileRecord ToCore(FilenameRecord record) => new(
        record.FileId,
        record.ParentId,
        record.Name,
        record.FullPath,
        record.SizeBytes,
        record.ModifiedUtc.Ticks,
        record.Flags);

    private bool TryGetBase(int id, out FilenameRecord? record)
    {
        if (baseTable is not null && baseTable.TryGet(id, out record)) return true;
        record = null;
        return false;
    }

    internal void LoadBase(string indexPath, string metadataPath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(indexPath);
        ArgumentException.ThrowIfNullOrWhiteSpace(metadataPath);
        RouteCEngine? next = null;
        ExactTable? table = null;
        Task<RouteCEngine> routeTask = Task.Run(() =>
        {
            var loaded = new RouteCEngine();
            try { loaded.Load(Path.GetFullPath(indexPath)); return loaded; }
            catch { loaded.Dispose(); throw; }
        });
        Task<ExactTable> metadataTask = Task.Run(() => ExactTable.Load(Path.GetFullPath(metadataPath), out _));
        try
        {
            Task.WaitAll(routeTask, metadataTask);
            next = routeTask.Result;
            table = metadataTask.Result;
            int[] routeIds = next.FileIds.ToArray();
            if (!table.Ids.AsSpan().SequenceEqual(routeIds))
                throw new InvalidDataException("Route C base/index metadata identity mismatch");
            lock (gate)
            {
                ThrowIfDisposed();
                baseEngine.Dispose();
                baseEngine = next;
                baseTable?.Dispose();
                baseTable = table;
                baseCount = table.Ids.Length;
                delta.Clear();
                deltaIndex.Clear();
            }
            next = null;
            table = null;
        }
        catch (AggregateException ex)
        {
            // Preserve the original failure type for GenerationStore's fail-safe rebuild
            // path while disposing whichever independent load completed successfully.
            if (routeTask.Status == TaskStatus.RanToCompletion) routeTask.Result.Dispose();
            if (metadataTask.Status == TaskStatus.RanToCompletion) metadataTask.Result.Dispose();
            throw ex.Flatten().InnerExceptions[0];
        }
        finally
        {
            next?.Dispose();
            table?.Dispose();
        }
    }

    internal bool TryGetRecord(int id, out FilenameRecord? record)
    {
        lock (gate)
        {
            ThrowIfDisposed();
            if (delta.TryGetValue(id, out record)) return record is not null;
            return baseTable is not null && baseTable.TryGet(id, out record);
        }
    }

    /// <summary>
    /// Reads the fixed-width metadata columns without decoding the exact name/path blobs.
    /// Reconcile uses this for every existing entry; full FilenameRecord materialization is
    /// reserved for the small subset whose filesystem metadata actually changed.
    /// </summary>
    internal StableMetadataSnapshot CreateStableMetadataSnapshot()
    {
        lock (gate)
        {
            ThrowIfDisposed();
            int maxId = Math.Max(0, baseTable?.MaxId ?? 0);
            foreach (int id in delta.Keys) maxId = Math.Max(maxId, id);
            var sizes = new ulong[checked(maxId + 1)];
            var ticks = new long[sizes.Length];
            var flags = new byte[sizes.Length];
            var present = new bool[sizes.Length];
            int live = 0;
            baseTable?.ForEachMetadata((id, size, modified, entryFlags) =>
            {
                sizes[id] = size; ticks[id] = modified; flags[id] = entryFlags;
                present[id] = true; live++;
            });
            foreach ((int id, FilenameRecord? record) in delta)
            {
                if (record is null)
                {
                    if (present[id]) { present[id] = false; live--; }
                    continue;
                }
                if (!present[id]) live++;
                sizes[id] = record.SizeBytes; ticks[id] = record.ModifiedUtc.Ticks;
                flags[id] = record.Flags; present[id] = true;
            }
            return new StableMetadataSnapshot(sizes, ticks, flags, present, live);
        }
    }

    internal int MaxFileId
    {
        get
        {
            lock (gate)
            {
                ThrowIfDisposed();
                int max = baseTable?.MaxId ?? 0;
                foreach (int id in delta.Keys) max = Math.Max(max, id);
                return max;
            }
        }
    }

    internal void ForEachRecord(Action<FilenameRecord> visitor)
    {
        ArgumentNullException.ThrowIfNull(visitor);
        lock (gate)
        {
            ThrowIfDisposed();
            HashSet<int> overridden = delta.Keys.ToHashSet();
            if (baseTable is not null)
                foreach (FilenameRecord record in baseTable.Records())
                    if (!overridden.Contains(record.FileId)) visitor(record);
            foreach (FilenameRecord? record in delta.Values)
                if (record is not null) visitor(record);
        }
    }

    internal void ForEachPath(Action<int, string> visitor)
    {
        ArgumentNullException.ThrowIfNull(visitor);
        lock (gate)
        {
            ThrowIfDisposed();
            if (baseTable is not null) baseTable.ForEachPath(visitor);
        }
    }

    internal void ForEachPathHash(Action<int, ulong> visitor)
    {
        ArgumentNullException.ThrowIfNull(visitor);
        lock (gate)
        {
            ThrowIfDisposed();
            HashSet<int>? overridden = delta.Count == 0 ? null : delta.Keys.ToHashSet();
            if (baseTable is not null)
                baseTable.ForEachPathHash((id, hash) =>
                {
                    if (overridden is null || !overridden.Contains(id)) visitor(id, hash);
                });
            foreach ((int id, FilenameRecord? record) in delta)
                if (record is not null) visitor(id, FileSystemCatalog.PathHash(record.FullPath));
        }
    }

    internal void ForEachNativePath(Action<FileKey, string> visitor)
    {
        ArgumentNullException.ThrowIfNull(visitor);
        lock (gate)
        {
            ThrowIfDisposed();
            HashSet<int>? overridden = delta.Count == 0 ? null : delta.Keys.ToHashSet();
            if (baseTable is not null)
                baseTable.ForEachNativePath((key, path, id) =>
                {
                    if (overridden is null || !overridden.Contains(id)) visitor(key, path);
                });
            foreach (FilenameRecord? record in delta.Values)
                if (record is { Key.IsNative: true }) visitor(record.Key, record.FullPath);
        }
    }

    private sealed class ExactTable : IDisposable
    {
        private readonly int[] ids, idToIndex, parentIds;
        private readonly bool[] hasParent, hasParentKey;
        private readonly FileKey[] keys, parentKeys;
        private readonly ulong[] sizes;
        private readonly long[] modifiedTicks;
        private readonly byte[] flags;
        private readonly int[] nameOffsets, nameLengths, pathOffsets, pathLengths;
        private readonly ulong[] pathHashes;
        private readonly byte[] nameBytes, pathBytes;
        private readonly Encoding utf8 = new UTF8Encoding(false);

        private ExactTable(int[] ids, int[] idToIndex, int[] parentIds, bool[] hasParent, bool[] hasParentKey,
            FileKey[] keys, FileKey[] parentKeys, ulong[] sizes, long[] modifiedTicks, byte[] flags,
            int[] nameOffsets, int[] nameLengths, int[] pathOffsets, int[] pathLengths,
            byte[] nameBytes, byte[] pathBytes, ulong[] pathHashes)
        {
            this.ids = ids; this.idToIndex = idToIndex; this.parentIds = parentIds; this.hasParent = hasParent;
            this.hasParentKey = hasParentKey; this.keys = keys; this.parentKeys = parentKeys; this.sizes = sizes;
            this.modifiedTicks = modifiedTicks; this.flags = flags; this.nameOffsets = nameOffsets; this.nameLengths = nameLengths;
            this.pathOffsets = pathOffsets; this.pathLengths = pathLengths; this.nameBytes = nameBytes; this.pathBytes = pathBytes; this.pathHashes = pathHashes;
        }

        public static ExactTable Create(IReadOnlyList<FilenameRecord> records, out int count)
        {
            IReadOnlyList<FilenameRecord> ordered = IsSortedByFileId(records)
                ? records
                : records.OrderBy(r => r.FileId).ToArray();
            count = ordered.Count;
            int maxId = ordered.Count == 0 ? 0 : ordered[^1].FileId;
            var ids = new int[ordered.Count]; var idToIndex = new int[checked(maxId + 1)];
            var parentIds = new int[ordered.Count]; var hasParent = new bool[ordered.Count]; var hasParentKey = new bool[ordered.Count];
            var keys = new FileKey[ordered.Count]; var parentKeys = new FileKey[ordered.Count]; var sizes = new ulong[ordered.Count];
            var ticks = new long[ordered.Count]; var flags = new byte[ordered.Count];
            var nameOffsets = new int[ordered.Count]; var nameLengths = new int[ordered.Count];
            var pathOffsets = new int[ordered.Count]; var pathLengths = new int[ordered.Count];
            var pathHashes = new ulong[ordered.Count];
            var encoding = new UTF8Encoding(false);
            int nameTotal = 0, pathTotal = 0;
            for (int i = 0; i < ordered.Count; i++)
            {
                FilenameRecord r = ordered[i];
                nameOffsets[i] = nameTotal; nameLengths[i] = encoding.GetByteCount(r.Name);
                pathOffsets[i] = pathTotal; pathLengths[i] = encoding.GetByteCount(r.FullPath);
                nameTotal = checked(nameTotal + nameLengths[i]);
                pathTotal = checked(pathTotal + pathLengths[i]);
            }
            var nameBytes = new byte[nameTotal];
            var pathBytes = new byte[pathTotal];
            for (int i = 0; i < ordered.Count; i++)
            {
                FilenameRecord r = ordered[i]; ids[i] = r.FileId; idToIndex[r.FileId] = i + 1;
                if (r.ParentId is int parent) { parentIds[i] = parent; hasParent[i] = true; }
                if (r.ParentKey is FileKey parentKey) { parentKeys[i] = parentKey; hasParentKey[i] = true; }
                keys[i] = r.Key; sizes[i] = r.SizeBytes; ticks[i] = r.ModifiedUtc.Ticks; flags[i] = r.Flags;
                encoding.GetBytes(r.Name, nameBytes.AsSpan(nameOffsets[i], nameLengths[i]));
                encoding.GetBytes(r.FullPath, pathBytes.AsSpan(pathOffsets[i], pathLengths[i]));
                pathHashes[i] = FileSystemCatalog.PathHash(r.FullPath);
            }
            return new ExactTable(ids, idToIndex, parentIds, hasParent, hasParentKey, keys, parentKeys, sizes, ticks, flags,
                nameOffsets, nameLengths, pathOffsets, pathLengths, nameBytes, pathBytes, pathHashes);
        }

        public static ExactTable Load(string path, out int count)
        {
            // Metadata is a fixed-width sequential file during restart. Use a large
            // sequential buffer so BinaryReader does not turn the million-entry header
            // pass into a stream of small random reads on a cold NVMe cache.
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read,
                FileShare.Read | FileShare.Delete, 1 << 20, FileOptions.SequentialScan);
            using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: true);
            string magic = reader.ReadString();
            if (magic == "PRFMETA5") return LoadCompactFast(path, out count);
            if (magic is not ("PRFMETA3" or "PRFMETA4")) throw new InvalidDataException("Filename metadata header mismatch");
            bool hasPathHashes = magic == "PRFMETA4";
            count = reader.ReadInt32();
            if (count < 0 || count > 20_000_000) throw new InvalidDataException("Filename metadata count invalid");
            var ids = new int[count];
            int maxId = 0, previousId = int.MinValue;
            var parentIds = new int[count]; var hasParent = new bool[count]; var hasParentKey = new bool[count];
            var keys = new FileKey[count]; var parentKeys = new FileKey[count]; var sizes = new ulong[count];
            var ticks = new long[count]; var flags = new byte[count];
            var nameOffsets = new int[count]; var nameLengths = new int[count];
            var pathOffsets = new int[count]; var pathLengths = new int[count];
            var pathHashes = new ulong[count];
            var names = new ArrayBufferWriter<byte>(); var paths = new ArrayBufferWriter<byte>();
            var encoding = new UTF8Encoding(false);
            var volumeIds = new Dictionary<string, string>(StringComparer.Ordinal);
            for (int i = 0; i < count; i++)
            {
                int id = reader.ReadInt32();
                if (id <= previousId) throw new InvalidDataException("Filename metadata ids are not strictly increasing");
                previousId = id; ids[i] = id; maxId = Math.Max(maxId, id);
                int parent = reader.ReadInt32(); hasParent[i] = reader.ReadBoolean(); parentIds[i] = parent;
                keys[i] = ReadKey(reader, volumeIds); hasParentKey[i] = reader.ReadBoolean();
                if (hasParentKey[i]) parentKeys[i] = ReadKey(reader, volumeIds);
                sizes[i] = reader.ReadUInt64(); ticks[i] = reader.ReadInt64(); flags[i] = reader.ReadByte();
                if (hasPathHashes)
                {
                    byte[] nameBytes = ReadUtf8(reader), pathBytes = ReadUtf8(reader);
                    nameOffsets[i] = names.WrittenCount; nameLengths[i] = nameBytes.Length;
                    nameBytes.AsSpan().CopyTo(names.GetSpan(nameBytes.Length)); names.Advance(nameBytes.Length);
                    pathOffsets[i] = paths.WrittenCount; pathLengths[i] = pathBytes.Length;
                    pathBytes.AsSpan().CopyTo(paths.GetSpan(pathBytes.Length)); paths.Advance(pathBytes.Length);
                    pathHashes[i] = reader.ReadUInt64();
                }
                else
                {
                    string name = reader.ReadString(), fullPath = reader.ReadString();
                    nameOffsets[i] = names.WrittenCount;
                    nameLengths[i] = encoding.GetBytes(name, names.GetSpan(encoding.GetByteCount(name)));
                    names.Advance(nameLengths[i]);
                    pathOffsets[i] = paths.WrittenCount;
                    pathLengths[i] = encoding.GetBytes(fullPath, paths.GetSpan(encoding.GetByteCount(fullPath)));
                    paths.Advance(pathLengths[i]);
                    pathHashes[i] = FileSystemCatalog.PathHash(fullPath);
                }
            }
            if (stream.Position != stream.Length) throw new InvalidDataException("Filename metadata trailing bytes");
            var idToIndex = new int[checked(maxId + 1)];
            for (int i = 0; i < ids.Length; i++) idToIndex[ids[i]] = i + 1;
            return new ExactTable(ids, idToIndex, parentIds, hasParent, hasParentKey, keys, parentKeys, sizes, ticks, flags,
                nameOffsets, nameLengths, pathOffsets, pathLengths, names.WrittenSpan.ToArray(), paths.WrittenSpan.ToArray(), pathHashes);

            static FileKey ReadKey(BinaryReader reader, Dictionary<string, string> volumeIds)
            {
                string volume = reader.ReadString();
                if (!volumeIds.TryGetValue(volume, out string? shared))
                    volumeIds[volume] = shared = volume;
                return new FileKey(shared, reader.ReadUInt64(), reader.ReadBoolean());
            }

            static byte[] ReadUtf8(BinaryReader reader)
            {
                int length = Read7BitInt(reader);
                if (length < 0 || length > 1_000_000) throw new InvalidDataException("Filename metadata string length invalid");
                byte[] value = reader.ReadBytes(length);
                if (value.Length != length) throw new EndOfStreamException();
                return value;
            }

            static int Read7BitInt(BinaryReader reader)
            {
                int value = 0, shift = 0;
                while (shift < 35)
                {
                    byte next = reader.ReadByte();
                    value |= (next & 0x7F) << shift;
                    if ((next & 0x80) == 0) return value;
                    shift += 7;
                }
                throw new InvalidDataException("Filename metadata string length encoding invalid");
            }

            // PRFMETA5 is the production format and has a fixed-width record section.  
            // Decode it from one sequential byte buffer instead of issuing roughly ten
            // million BinaryReader calls and growing two intermediate blob buffers.  The
            // loaded table keeps one backing array for both exact UTF-8 blobs; offsets are
            // translated to absolute positions after all structural checks complete.
            static ExactTable LoadCompactFast(string path, out int count)
            {
                byte[] data = File.ReadAllBytes(path);
                int offset = 0;
                string magic = ReadString(data, ref offset);
                if (!magic.Equals("PRFMETA5", StringComparison.Ordinal))
                    throw new InvalidDataException("Filename metadata header mismatch");
                count = ReadInt32(data, ref offset);
                if (count < 0 || count > 20_000_000) throw new InvalidDataException("Filename metadata count invalid");
                int volumeCount = ReadInt32(data, ref offset);
                if (volumeCount < 0 || volumeCount > 256) throw new InvalidDataException("Filename metadata volume count invalid");
                var volumes = new string[volumeCount];
                for (int i = 0; i < volumes.Length; i++) volumes[i] = ReadString(data, ref offset);

                var ids = new int[count]; int maxId = 0, previousId = int.MinValue;
                var parentIds = new int[count]; var hasParent = new bool[count]; var hasParentKey = new bool[count];
                var keys = new FileKey[count]; var parentKeys = new FileKey[count]; var sizes = new ulong[count];
                var ticks = new long[count]; var flags = new byte[count]; var pathHashes = new ulong[count];
                var nameOffsets = new int[count]; var nameLengths = new int[count];
                var pathOffsets = new int[count]; var pathLengths = new int[count];
                for (int i = 0; i < count; i++)
                {
                    int id = ReadInt32(data, ref offset);
                    if (id <= previousId) throw new InvalidDataException("Filename metadata ids are not strictly increasing");
                    previousId = id; ids[i] = id; maxId = Math.Max(maxId, id);
                    parentIds[i] = ReadInt32(data, ref offset); hasParent[i] = ReadBoolean(data, ref offset);
                    keys[i] = ReadCompactKey(data, ref offset, volumes);
                    hasParentKey[i] = ReadBoolean(data, ref offset);
                    if (hasParentKey[i]) parentKeys[i] = ReadCompactKey(data, ref offset, volumes);
                    sizes[i] = ReadUInt64(data, ref offset); ticks[i] = ReadInt64(data, ref offset); flags[i] = ReadByte(data, ref offset);
                    pathHashes[i] = ReadUInt64(data, ref offset);
                    nameOffsets[i] = ReadInt32(data, ref offset); nameLengths[i] = ReadInt32(data, ref offset);
                    pathOffsets[i] = ReadInt32(data, ref offset); pathLengths[i] = ReadInt32(data, ref offset);
                }
                int nameBytesLength = ReadInt32(data, ref offset), pathBytesLength = ReadInt32(data, ref offset);
                if (nameBytesLength < 0 || pathBytesLength < 0 || nameBytesLength > 2_000_000_000 || pathBytesLength > 2_000_000_000)
                    throw new InvalidDataException("Filename metadata blob length invalid");
                int nameStart = offset;
                EnsureAvailable(data, offset, nameBytesLength);
                offset = checked(offset + nameBytesLength);
                int pathStart = offset;
                EnsureAvailable(data, offset, pathBytesLength);
                offset = checked(offset + pathBytesLength);
                if (offset != data.Length) throw new InvalidDataException("Filename metadata blob is truncated");
                for (int i = 0; i < count; i++)
                {
                    if (nameOffsets[i] < 0 || nameLengths[i] < 0 || pathOffsets[i] < 0 || pathLengths[i] < 0 ||
                        nameOffsets[i] > nameBytesLength - nameLengths[i] || pathOffsets[i] > pathBytesLength - pathLengths[i])
                        throw new InvalidDataException("Filename metadata string bounds invalid");
                    nameOffsets[i] = checked(nameStart + nameOffsets[i]);
                    pathOffsets[i] = checked(pathStart + pathOffsets[i]);
                }
                var idToIndex = new int[checked(maxId + 1)];
                for (int i = 0; i < ids.Length; i++) idToIndex[ids[i]] = i + 1;
                return new ExactTable(ids, idToIndex, parentIds, hasParent, hasParentKey, keys, parentKeys, sizes, ticks, flags,
                    nameOffsets, nameLengths, pathOffsets, pathLengths, data, data, pathHashes);

                static int ReadInt32(byte[] data, ref int offset)
                {
                    EnsureAvailable(data, offset, sizeof(int));
                    int value = BinaryPrimitives.ReadInt32LittleEndian(data.AsSpan(offset, sizeof(int)));
                    offset += sizeof(int); return value;
                }
                static long ReadInt64(byte[] data, ref int offset)
                {
                    EnsureAvailable(data, offset, sizeof(long));
                    long value = BinaryPrimitives.ReadInt64LittleEndian(data.AsSpan(offset, sizeof(long)));
                    offset += sizeof(long); return value;
                }
                static ulong ReadUInt64(byte[] data, ref int offset)
                {
                    EnsureAvailable(data, offset, sizeof(ulong));
                    ulong value = BinaryPrimitives.ReadUInt64LittleEndian(data.AsSpan(offset, sizeof(ulong)));
                    offset += sizeof(ulong); return value;
                }
                static byte ReadByte(byte[] data, ref int offset)
                {
                    EnsureAvailable(data, offset, 1); return data[offset++];
                }
                static bool ReadBoolean(byte[] data, ref int offset) => ReadByte(data, ref offset) != 0;
                static string ReadString(byte[] data, ref int offset)
                {
                    int length = Read7BitInt(data, ref offset);
                    if (length < 0 || length > 1_000_000) throw new InvalidDataException("Filename metadata string length invalid");
                    EnsureAvailable(data, offset, length);
                    string value = Encoding.UTF8.GetString(data, offset, length);
                    offset += length; return value;
                }
                static int Read7BitInt(byte[] data, ref int offset)
                {
                    uint value = 0;
                    for (int shift = 0; shift < 35; shift += 7)
                    {
                        byte next = ReadByte(data, ref offset);
                        value |= (uint)(next & 0x7F) << shift;
                        if ((next & 0x80) == 0)
                        {
                            if (value > int.MaxValue) throw new InvalidDataException("Filename metadata string length encoding invalid");
                            return (int)value;
                        }
                    }
                    throw new InvalidDataException("Filename metadata string length encoding invalid");
                }
                static FileKey ReadCompactKey(byte[] data, ref int offset, IReadOnlyList<string> volumes)
                {
                    int volume = ReadInt32(data, ref offset);
                    if ((uint)volume >= (uint)volumes.Count) throw new InvalidDataException("Filename metadata volume index invalid");
                    ulong nativeId = ReadUInt64(data, ref offset);
                    bool native = ReadBoolean(data, ref offset);
                    return new FileKey(volumes[volume], nativeId, native);
                }
                static void EnsureAvailable(byte[] data, int offset, int length)
                {
                    if (offset < 0 || length < 0 || offset > data.Length - length)
                        throw new EndOfStreamException();
                }
            }

        }

        public int MaxId => ids.Length == 0 ? 0 : ids[^1];
        public int[] Ids => ids;

        public bool TryGet(int id, out FilenameRecord? record)
        {
            if ((uint)id >= (uint)idToIndex.Length || idToIndex[id] == 0) { record = null; return false; }
            record = Get(idToIndex[id] - 1); return true;
        }

        public void ForEachMetadata(Action<int, ulong, long, byte> visitor)
        {
            ArgumentNullException.ThrowIfNull(visitor);
            for (int i = 0; i < ids.Length; i++)
                visitor(ids[i], sizes[i], modifiedTicks[i], flags[i]);
        }

        public IEnumerable<FilenameRecord> Records()
        {
            for (int i = 0; i < ids.Length; i++) yield return Get(i);
        }

        public void ForEachPath(Action<int, string> visitor)
        {
            for (int i = 0; i < ids.Length; i++)
                visitor(ids[i], utf8.GetString(pathBytes, pathOffsets[i], pathLengths[i]));
        }

        public void ForEachPathHash(Action<int, ulong> visitor)
        {
            for (int i = 0; i < ids.Length; i++) visitor(ids[i], pathHashes[i]);
        }

        public void ForEachNativePath(Action<FileKey, string> visitor)
        {
            for (int i = 0; i < ids.Length; i++)
                if (keys[i].IsNative) visitor(keys[i], utf8.GetString(pathBytes, pathOffsets[i], pathLengths[i]));
        }

        public void ForEachNativePath(Action<FileKey, string, int> visitor)
        {
            for (int i = 0; i < ids.Length; i++)
                if (keys[i].IsNative) visitor(keys[i], utf8.GetString(pathBytes, pathOffsets[i], pathLengths[i]), ids[i]);
        }

        private FilenameRecord Get(int i)
        {
            string name = utf8.GetString(nameBytes, nameOffsets[i], nameLengths[i]);
            string path = utf8.GetString(pathBytes, pathOffsets[i], pathLengths[i]);
            return new FilenameRecord(ids[i], hasParent[i] ? parentIds[i] : null, name, path, sizes[i],
                new DateTime(modifiedTicks[i], DateTimeKind.Utc), flags[i]) { Key = keys[i], ParentKey = hasParentKey[i] ? parentKeys[i] : null };
        }

        public void Dispose() { }
    }

    private static bool Matches(FilenameRecord record, SearchRequest request)
    {
        string target = request.Scope == SearchScope.Filename ? record.Name : record.FullPath;
        return global::FilenameSearch.Core.FilenameSemantics.Matches(target, request.Query, request.CaseSensitive);
    }

    private static bool HasWildcard(string query) =>
        query.AsSpan().IndexOfAny('*', '?') >= 0;

    /// <summary>
    /// Creates a conservative literal AND query for the immutable Route C base. Every true
    /// wildcard match must contain all returned literal segments, so this stage may add false
    /// positives but cannot remove a true result. The exact substring-glob semantics are applied
    /// after Route C returns candidates.
    /// </summary>
    private static string BuildBroadLiteralQuery(string query)
    {
        var literals = new List<string>();
        foreach (string token in query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            if (token.IndexOfAny(['*', '?']) < 0)
            {
                literals.Add(token);
                continue;
            }
            var segment = new StringBuilder();
            foreach (Rune rune in token.EnumerateRunes())
            {
                if (rune.Value is '*' or '?')
                {
                    if (segment.Length > 0) { literals.Add(segment.ToString()); segment.Clear(); }
                }
                else segment.Append(rune.ToString());
            }
            if (segment.Length > 0) literals.Add(segment.ToString());
        }
        return string.Join(' ', literals);
    }

    private static bool IsSortedByFileId(IReadOnlyList<FilenameRecord> records)
    {
        int previous = int.MinValue;
        for (int i = 0; i < records.Count; i++)
        {
            int current = records[i].FileId;
            if (current <= previous) return false;
            previous = current;
        }
        return true;
    }

    private static void ValidateExact(IReadOnlyList<FilenameRecord> records)
    {
        // IsSortedByFileId also proves uniqueness for the production path. Keep a bounded
        // fallback for unsorted external callers without allocating a LINQ iterator chain.
        if (IsSortedByFileId(records)) return;
        var seen = new HashSet<int>();
        for (int i = 0; i < records.Count; i++)
            if (!seen.Add(records[i].FileId)) throw new ArgumentException("FileId values must be unique", nameof(records));
        // FileKey identifies the underlying file object. Multiple hard-link directory entries
        // may legitimately share one native FileKey while retaining distinct exact paths/FileIds.
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);

    /// <summary>Small n-gram index for the mutable delta. It keeps normal post-update queries off a full base scan.</summary>
    private sealed class DeltaIndex
    {
        private readonly Dictionary<int, Prepared> current = [];
        private readonly Dictionary<string, HashSet<int>> name = new(StringComparer.Ordinal);
        private readonly Dictionary<string, HashSet<int>> path = new(StringComparer.Ordinal);

        public int LastCandidateCount { get; private set; }
        public bool LastUsedScan { get; private set; }

        public void Clear()
        {
            current.Clear(); name.Clear(); path.Clear();
            LastCandidateCount = 0; LastUsedScan = false;
        }

        public void Remove(int id)
        {
            if (!current.Remove(id, out Prepared previous)) return;
            RemoveKeys(name, previous.NameKeys, id);
            RemoveKeys(path, previous.PathKeys, id);
        }

        public void Upsert(FilenameRecord record)
        {
            Remove(record.FileId);
            string nf = global::FilenameSearch.Core.FilenameSemantics.Normalize(record.Name, false);
            string pf = global::FilenameSearch.Core.FilenameSemantics.Normalize(record.FullPath, false);
            string[] nk = Ngrams(nf).Distinct(StringComparer.Ordinal).ToArray();
            string[] pk = Ngrams(pf).Distinct(StringComparer.Ordinal).ToArray();
            AddKeys(name, nk, record.FileId);
            AddKeys(path, pk, record.FileId);
            current[record.FileId] = new Prepared(record, nk, pk);
        }

        public IReadOnlyList<FilenameRecord> Search(
            SearchRequest request,
            IReadOnlyDictionary<int, FilenameRecord> live)
        {
            if (live.Count == 0)
            {
                LastCandidateCount = 0; LastUsedScan = false; return [];
            }
            string foldedQuery = global::FilenameSearch.Core.FilenameSemantics.Normalize(request.Query, false);
            HashSet<int>? candidates = null;
            foreach (string token in foldedQuery.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
            {
                string[] literalSegments = token.IndexOfAny(['*', '?']) >= 0
                    ? LiteralSegments(token)
                    : [token];
                foreach (string segment in literalSegments)
                {
                    string? key = BestKey(segment);
                    if (key is null) continue;
                    var table = request.Scope == SearchScope.Filename ? name : path;
                    if (!table.TryGetValue(key, out HashSet<int>? ids))
                    {
                        LastCandidateCount = 0; LastUsedScan = false; return [];
                    }
                    candidates = candidates is null ? new HashSet<int>(ids) : IntersectInPlace(candidates, ids);
                    if (candidates.Count == 0)
                    {
                        LastCandidateCount = 0; LastUsedScan = false; return [];
                    }
                }
            }

            IEnumerable<int> idsToCheck;
            if (candidates is null)
            {
                LastUsedScan = true;
                idsToCheck = live.Keys;
                LastCandidateCount = live.Count;
            }
            else
            {
                LastUsedScan = false;
                idsToCheck = candidates;
                LastCandidateCount = candidates.Count;
            }
            var result = new List<FilenameRecord>();
            foreach (int id in idsToCheck)
            {
                if (!live.TryGetValue(id, out FilenameRecord? record)) continue;
                if (Matches(record, request)) result.Add(record);
            }
            return result;
        }

        private static string? BestKey(string segment)
        {
            Rune[] runes = segment.EnumerateRunes().ToArray();
            int length = Math.Min(3, runes.Length);
            if (length == 0) return null;
            var b = new StringBuilder();
            for (int i = 0; i < length; i++) b.Append(runes[i].ToString());
            return b.ToString();
        }

        private static string[] LiteralSegments(string token)
        {
            var result = new List<string>();
            var b = new StringBuilder();
            foreach (Rune r in token.EnumerateRunes())
            {
                if (r.Value is '*' or '?')
                {
                    if (b.Length > 0) { result.Add(b.ToString()); b.Clear(); }
                }
                else b.Append(r.ToString());
            }
            if (b.Length > 0) result.Add(b.ToString());
            return result.ToArray();
        }

        private static IEnumerable<string> Ngrams(string value)
        {
            Rune[] runes = value.EnumerateRunes().ToArray();
            for (int len = 1; len <= 3; len++)
                for (int i = 0; i + len <= runes.Length; i++)
                {
                    var b = new StringBuilder();
                    for (int j = 0; j < len; j++) b.Append(runes[i + j].ToString());
                    yield return b.ToString();
                }
        }

        private static void AddKeys(Dictionary<string, HashSet<int>> table, IEnumerable<string> keys, int id)
        {
            foreach (string key in keys)
            {
                if (!table.TryGetValue(key, out HashSet<int>? set)) table[key] = set = [];
                set.Add(id);
            }
        }

        private static void RemoveKeys(Dictionary<string, HashSet<int>> table, IEnumerable<string> keys, int id)
        {
            foreach (string key in keys)
            {
                if (!table.TryGetValue(key, out HashSet<int>? set)) continue;
                set.Remove(id);
                if (set.Count == 0) table.Remove(key);
            }
        }

        private static HashSet<int> IntersectInPlace(HashSet<int> left, HashSet<int> right)
        {
            left.IntersectWith(right);
            return left;
        }

        private readonly record struct Prepared(FilenameRecord Record, string[] NameKeys, string[] PathKeys);
    }
}

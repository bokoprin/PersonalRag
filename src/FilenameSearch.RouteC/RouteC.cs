using System.Buffers;
using System.Diagnostics;
using System.Text;
using FilenameSearch.Core;

namespace FilenameSearch.RouteC;

/// <summary>
/// Production Route C immutable candidate index. Search results are conservative candidates;
/// the product adapter always performs exact verification against exact filesystem metadata.
/// One/two-rune grams are complete. Selective trigrams reduce rare-query candidate sets while
/// common trigrams safely fall back to the complete shorter grams.
/// </summary>
public sealed class RouteCEngine : IFilenameSearchEngine, IDisposable
{
    private const string Magic = "PRFRC005";
    private const int Version = 5;
    private readonly object gate = new();
    private int[] ids = [];
    private CompactIndex nameIndex = CompactIndex.Empty;
    private CompactIndex pathIndex = CompactIndex.Empty;
    private readonly Dictionary<int, FileRecord?> overlay = [];
    private bool disposed;

    public string RouteName => "C";
    public IReadOnlyList<int> FileIds { get { lock (gate) { ThrowIfDisposed(); return ids.ToArray(); } } }

    public IReadOnlyList<FileRecord> Records
    {
        get
        {
            lock (gate)
            {
                ThrowIfDisposed();
                var result = new List<FileRecord>(ids.Length + overlay.Count);
                foreach (int id in ids)
                {
                    if (overlay.TryGetValue(id, out FileRecord? changed))
                    {
                        if (changed is not null) result.Add(changed);
                    }
                    else result.Add(Minimal(id));
                }
                foreach ((int id, FileRecord? changed) in overlay)
                {
                    if (Array.BinarySearch(ids, id) < 0 && changed is not null) result.Add(changed);
                }
                result.Sort(static (a, b) => a.FileId.CompareTo(b.FileId));
                return result;
            }
        }
    }

    public void Build(IReadOnlyList<FileRecord> source)
    {
        ArgumentNullException.ThrowIfNull(source);
        FileRecord[] ordered = source.OrderBy(r => r.FileId).ToArray();
        ValidateIds(ordered);
        string[] names = new string[ordered.Length];
        string[] paths = new string[ordered.Length];
        int[] fileIds = new int[ordered.Length];
        for (int i = 0; i < ordered.Length; i++)
        {
            fileIds[i] = ordered[i].FileId;
            names[i] = FilenameSemantics.Normalize(ordered[i].Name, false);
            paths[i] = FilenameSemantics.Normalize(ordered[i].FullPath, false);
        }
        BuildIndexes(fileIds, names, paths);
    }

    /// <summary>Builds the immutable indexes from the exact adapter without allocating a parallel
    /// million-record Core FileRecord graph. The public Core engine overload remains available
    /// for standalone callers and tests.</summary>
    public void Build(IReadOnlyList<int> fileIds, IReadOnlyList<string> normalizedNames,
        IReadOnlyList<string> normalizedPaths)
    {
        ArgumentNullException.ThrowIfNull(fileIds);
        ArgumentNullException.ThrowIfNull(normalizedNames);
        ArgumentNullException.ThrowIfNull(normalizedPaths);
        if (fileIds.Count != normalizedNames.Count || fileIds.Count != normalizedPaths.Count)
            throw new ArgumentException("Route C build arrays must have equal lengths");
        int[] idsCopy = fileIds.ToArray();
        ValidateSortedIds(idsCopy);
        BuildIndexes(idsCopy, normalizedNames, normalizedPaths);
    }

    /// <summary>Adapter-friendly build that keeps only one normalization array at a time.
    /// Selectors are evaluated during the two sequential index passes, avoiding both the
    /// Core FileRecord graph and simultaneous normalized name/path arrays.</summary>
    public void Build(IReadOnlyList<int> fileIds, Func<int, string> nameSelector,
        Func<int, string> pathSelector)
    {
        ArgumentNullException.ThrowIfNull(fileIds);
        ArgumentNullException.ThrowIfNull(nameSelector);
        ArgumentNullException.ThrowIfNull(pathSelector);
        int[] idsCopy = fileIds.ToArray();
        ValidateSortedIds(idsCopy);
        CompactIndex nextNames = BuildIndex(idsCopy.Length,
            i => FilenameSemantics.Normalize(nameSelector(i), false),
            includeOneRune: true, includeShort: true, componentsOnly: false);
        GC.Collect(2, GCCollectionMode.Forced, blocking: true, compacting: false);
        CompactIndex nextPaths = BuildIndex(idsCopy.Length,
            i => FilenameSemantics.Normalize(pathSelector(i), false),
            includeOneRune: false, includeShort: false, componentsOnly: true);
        lock (gate)
        {
            ThrowIfDisposed();
            ids = idsCopy;
            nameIndex = nextNames;
            pathIndex = nextPaths;
            overlay.Clear();
        }
    }

    private void BuildIndexes(IReadOnlyList<int> fileIds, IReadOnlyList<string> names, IReadOnlyList<string> paths)
    {
        CompactIndex nextNames = BuildIndex(names, includeOneRune: true, includeShort: true, componentsOnly: false);
        // Build the two indexes sequentially. The immutable index owns compact flat posting
        // arrays; retaining two normalization arrays and two temporary count maps at once
        // would otherwise dominate the 1M ready-memory gate.
        GC.Collect(2, GCCollectionMode.Forced, blocking: true, compacting: false);
        CompactIndex nextPaths = BuildIndex(paths, includeOneRune: false, includeShort: false, componentsOnly: true);
        lock (gate)
        {
            ThrowIfDisposed();
            ids = fileIds.ToArray();
            nameIndex = nextNames;
            pathIndex = nextPaths;
            overlay.Clear();
        }
    }

    public void Save(string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string full = Path.GetFullPath(store);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        int[] idSnapshot;
        CompactIndex nameSnapshot;
        CompactIndex pathSnapshot;
        lock (gate)
        {
            ThrowIfDisposed();
            if (overlay.Count != 0)
                throw new InvalidOperationException("Route C immutable store cannot be saved with an overlay");
            idSnapshot = ids.ToArray();
            nameSnapshot = nameIndex;
            pathSnapshot = pathIndex;
        }

        using var stream = new FileStream(full, FileMode.Create, FileAccess.Write, FileShare.Read, 1 << 20, FileOptions.SequentialScan);
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true);
        writer.Write(Magic);
        writer.Write(Version);
        writer.Write(idSnapshot.Length);
        foreach (int id in idSnapshot) writer.Write(id);
        WriteIndex(writer, nameSnapshot);
        WriteIndex(writer, pathSnapshot);
        writer.Flush();
        stream.Flush(flushToDisk: true);
    }

    public void Load(string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string full = Path.GetFullPath(store);
        using var stream = new FileStream(full, FileMode.Open, FileAccess.Read, FileShare.Read | FileShare.Delete, 1 << 20, FileOptions.SequentialScan);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: true);
        if (reader.ReadString() != Magic || reader.ReadInt32() != Version)
            throw new InvalidDataException("Route C store header mismatch");
        int count = reader.ReadInt32();
        if (count < 0 || count > 20_000_000) throw new InvalidDataException("Route C id count invalid");
        var nextIds = new int[count];
        for (int i = 0; i < count; i++) nextIds[i] = reader.ReadInt32();
        ValidateSortedIds(nextIds);
        CompactIndex nextNames = ReadIndex(reader);
        CompactIndex nextPaths = ReadIndex(reader);
        if (stream.Position != stream.Length) throw new InvalidDataException("Route C store has trailing bytes");
        // The immutable base file is verified by GenerationStore's SHA-256 before this
        // method is called. Decoding every posting list here made restart/load scale with
        // the total index rather than the bytes read; posting bounds are checked lazily by
        // Posting.ToArray when a candidate list is actually used. Header/count validation
        // above still rejects truncated or structurally impossible records immediately.

        lock (gate)
        {
            ThrowIfDisposed();
            ids = nextIds;
            nameIndex = nextNames;
            pathIndex = nextPaths;
            overlay.Clear();
        }
    }

    public SearchResult Search(FilenameQuery request)
    {
        ArgumentNullException.ThrowIfNull(request);
        var watch = Stopwatch.StartNew();
        lock (gate)
        {
            ThrowIfDisposed();
            string[] tokens = request.Query
                .Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries)
                .Select(t => FilenameSemantics.Normalize(t, false))
                .ToArray();

            CompactIndex index = request.Scope == FilenameScope.Filename ? nameIndex : pathIndex;
            // The path index is component-based to keep a million-entry store compact. A
            // query containing a separator may span the boundary between two components;
            // use the conservative full candidate set for that uncommon direct form.
            int[]? candidates = request.Scope == FilenameScope.FullPath &&
                tokens.Any(t => t.Contains('\\') || t.Contains('/'))
                ? null
                : CandidateIndexes(tokens, index);
            bool usedScan = candidates is null;
            int candidateCount = candidates?.Length ?? ids.Length;

            if (overlay.Count == 0)
            {
                IReadOnlyList<FileRecord> lazy = new CandidateRecordList(ids, candidates, request.Limit);
                watch.Stop();
                return new SearchResult(lazy, watch.Elapsed.TotalMilliseconds, candidateCount, usedScan);
            }

            // Historical benchmark compatibility only. Production never mutates this base.
            var materialized = new List<FileRecord>();
            IEnumerable<int> baseIndexes = candidates ?? Enumerable.Range(0, ids.Length);
            foreach (int baseIndex in baseIndexes)
            {
                int id = ids[baseIndex];
                if (overlay.ContainsKey(id)) continue;
                materialized.Add(Minimal(id));
                if (request.Limit > 0 && materialized.Count >= request.Limit) break;
            }
            if (request.Limit == 0 || materialized.Count < request.Limit)
            {
                foreach ((int _, FileRecord? changed) in overlay.OrderBy(p => p.Key))
                {
                    if (changed is null || !FilenameSemantics.Matches(changed, request)) continue;
                    materialized.Add(changed);
                    if (request.Limit > 0 && materialized.Count >= request.Limit) break;
                }
            }
            watch.Stop();
            return new SearchResult(materialized, watch.Elapsed.TotalMilliseconds, candidateCount + overlay.Count, usedScan);
        }
    }

    public void Upsert(FileRecord record)
    {
        ArgumentNullException.ThrowIfNull(record);
        lock (gate)
        {
            ThrowIfDisposed();
            overlay[record.FileId] = record;
        }
    }

    public bool Remove(int fileId)
    {
        lock (gate)
        {
            ThrowIfDisposed();
            bool existed = Array.BinarySearch(ids, fileId) >= 0 ||
                (overlay.TryGetValue(fileId, out FileRecord? current) && current is not null);
            if (!existed) return false;
            overlay[fileId] = null;
            return true;
        }
    }

    public void Dispose()
    {
        lock (gate)
        {
            if (disposed) return;
            disposed = true;
            ids = [];
            nameIndex = CompactIndex.Empty;
            pathIndex = CompactIndex.Empty;
            overlay.Clear();
        }
        GC.SuppressFinalize(this);
    }

    private static CompactIndex BuildIndex(IReadOnlyList<string> values, bool includeOneRune, bool includeShort, bool componentsOnly)
    {
        // Count retained n-grams first, then materialize every posting in one flat int array.
        // The old Dictionary<key,List<int>> representation allocated one object and one
        // backing array per key; for a million-entry corpus those object headers exceeded
        // the ready-memory gate even though the postings themselves were small.
        var counts = new Dictionary<ulong, int>();
        const int mediumDivisor = 32;
        int medium = Math.Max(4_096, Math.Max(1, values.Count / mediumDivisor));
        for (int i = 0; i < values.Count; i++)
        {
            foreach (ulong key in KeysFor(values[i], includeOneRune, includeShort, componentsOnly))
            {
                if (counts.TryGetValue(key, out int count)) counts[key] = count + 1;
                else counts[key] = 1;
            }
        }

        var selected = counts.Where(pair => pair.Value <= medium)
            .Select(pair => (Key: pair.Key, Count: pair.Value))
            .OrderBy(pair => pair.Key)
            .ToArray();
        var keys = new ulong[selected.Length];
        var postingCounts = new int[selected.Length];
        var offsets = new int[selected.Length + 1];
        for (int i = 0; i < selected.Length; i++)
        {
            keys[i] = selected[i].Key;
            postingCounts[i] = selected[i].Count;
            offsets[i + 1] = checked(offsets[i] + postingCounts[i]);
            // Reuse the count map as a compact lookup map. A negative value marks an
            // intentionally omitted high-frequency key during the second pass.
            counts[keys[i]] = i;
        }
        // Mark non-selected keys without retaining a second key set. Selected values are
        // their non-negative posting index; every other original count becomes -1.
        ulong[] allKeys = counts.Keys.ToArray();
        foreach (ulong key in allKeys) counts[key] = -1;
        for (int i = 0; i < keys.Length; i++) counts[keys[i]] = i;

        var valuesFlat = new int[offsets[^1]];
        var cursors = offsets[..^1].ToArray();
        for (int i = 0; i < values.Count; i++)
        {
            foreach (ulong key in KeysFor(values[i], includeOneRune, includeShort, componentsOnly))
            {
                if (!counts.TryGetValue(key, out int index) || index < 0) continue;
                valuesFlat[cursors[index]++] = i;
            }
        }
        // Encode delta postings once into one shared byte array. The ready-state index keeps
        // this compact representation instead of a four-byte int for every posting.
        var encoded = new ArrayBufferWriter<byte>();
        var byteOffsets = new int[keys.Length + 1];
        for (int i = 0; i < keys.Length; i++)
        {
            int previous = 0;
            int start = offsets[i];
            int end = offsets[i + 1];
            for (int p = start; p < end; p++)
            {
                uint delta = checked((uint)(valuesFlat[p] - previous));
                do
                {
                    Span<byte> span = encoded.GetSpan(1);
                    byte next = (byte)(delta & 0x7F);
                    delta >>= 7;
                    if (delta != 0) next |= 0x80;
                    span[0] = next;
                    encoded.Advance(1);
                } while (delta != 0);
                previous = valuesFlat[p];
            }
            byteOffsets[i + 1] = encoded.WrittenCount;
        }
        return new CompactIndex(keys, byteOffsets, postingCounts, encoded.WrittenSpan.ToArray());
    }

    private static CompactIndex BuildIndex(int count, Func<int, string> valueSelector,
        bool includeOneRune, bool includeShort, bool componentsOnly)
    {
        ArgumentNullException.ThrowIfNull(valueSelector);
        // Adapter builds use the exact metadata table as the source. Normalize one value at
        // a time in both passes instead of retaining one million normalized strings beside
        // the immutable exact records and posting maps.
        var counts = new Dictionary<ulong, int>();
        const int mediumDivisor = 32;
        int medium = Math.Max(4_096, Math.Max(1, count / mediumDivisor));
        for (int i = 0; i < count; i++)
        {
            foreach (ulong key in KeysFor(valueSelector(i), includeOneRune, includeShort, componentsOnly))
            {
                if (counts.TryGetValue(key, out int current))
                {
                    if (current <= medium) counts[key] = current + 1;
                }
                else counts[key] = 1;
            }
        }
        var selected = counts.Where(pair => pair.Value <= medium)
            .Select(pair => (Key: pair.Key, Count: pair.Value))
            .OrderBy(pair => pair.Key).ToArray();
        var keys = new ulong[selected.Length]; var postingCounts = new int[selected.Length];
        var offsets = new int[selected.Length + 1];
        for (int i = 0; i < selected.Length; i++)
        {
            keys[i] = selected[i].Key; postingCounts[i] = selected[i].Count;
            offsets[i + 1] = checked(offsets[i] + postingCounts[i]); counts[keys[i]] = i;
        }
        foreach (ulong key in counts.Keys.ToArray()) counts[key] = -1;
        for (int i = 0; i < keys.Length; i++) counts[keys[i]] = i;
        var valuesFlat = new int[offsets[^1]]; var cursors = offsets[..^1].ToArray();
        for (int i = 0; i < count; i++)
        {
            foreach (ulong key in KeysFor(valueSelector(i), includeOneRune, includeShort, componentsOnly))
                if (counts.TryGetValue(key, out int index) && index >= 0) valuesFlat[cursors[index]++] = i;
        }
        var encoded = new ArrayBufferWriter<byte>(); var byteOffsets = new int[keys.Length + 1];
        for (int i = 0; i < keys.Length; i++)
        {
            int previous = 0;
            for (int p = offsets[i]; p < offsets[i + 1]; p++)
            {
                uint delta = checked((uint)(valuesFlat[p] - previous));
                do
                {
                    Span<byte> span = encoded.GetSpan(1); byte next = (byte)(delta & 0x7F);
                    delta >>= 7; if (delta != 0) next |= 0x80; span[0] = next; encoded.Advance(1);
                } while (delta != 0);
                previous = valuesFlat[p];
            }
            byteOffsets[i + 1] = encoded.WrittenCount;
        }
        return new CompactIndex(keys, byteOffsets, postingCounts, encoded.WrittenSpan.ToArray());
    }

    private static IEnumerable<ulong> KeysFor(string value, bool includeOneRune, bool includeShort, bool componentsOnly)
    {
        var seen = new HashSet<ulong>();
        IEnumerable<string> parts = componentsOnly
            ? value.Split(['\\', '/'], StringSplitOptions.RemoveEmptyEntries)
            : [value];
        foreach (string part in parts)
        {
            Rune[] runes = part.EnumerateRunes().ToArray();
            if (includeShort && includeOneRune)
                for (int p = 0; p < runes.Length; p++) seen.Add(Pack(runes, p, 1));
            if (includeShort)
                for (int p = 0; p + 2 <= runes.Length; p++) seen.Add(Pack(runes, p, 2));
            for (int p = 0; p + 3 <= runes.Length; p++)
            {
                ulong key = Pack(runes, p, 3);
                if (TrackTrigram(runes, p)) seen.Add(key);
            }
        }
        return seen;
    }

    private static bool TrackTrigram(IReadOnlyList<Rune> runes, int start)
    {
        // Keep every trigram that carries a Unicode or path separator signal. Plain
        // alphanumeric keys may be omitted and therefore become an unconstrained candidate
        // that falls back to exact verification; this bounds index cardinality without
        // changing search correctness.
        for (int i = start; i < start + 3; i++)
        {
            int value = runes[i].Value;
            if (value > 0x7F || value is '_' or '.' or '-' or '(' or ')') return true;
        }
        return false;
    }

    private static int[]? CandidateIndexes(string[] tokens, CompactIndex index)
    {
        if (tokens.Length == 0) return null;
        int[]? intersection = null;
        foreach (string token in tokens)
        {
            if (token.IndexOfAny(['*', '?']) >= 0)
            {
                // Product adapter strips wildcard operators before Route C. Keeping this
                // fallback conservative prevents accidental false negatives for direct callers.
                return null;
            }
            Rune[] runes = token.EnumerateRunes().ToArray();
            if (runes.Length == 0) continue;

            var postings = new List<Posting>();
            if (runes.Length >= 3)
            {
                for (int i = 0; i + 3 <= runes.Length; i++)
                {
                    ulong key = Pack(runes, i, 3);
                    if (index.TryGet(key, out Posting posting)) postings.Add(posting);
                }
            }
            if (postings.Count == 0 && runes.Length >= 2)
            {
                for (int i = 0; i + 2 <= runes.Length; i++)
                {
                    ulong key = Pack(runes, i, 2);
                    if (index.TryGet(key, out Posting posting)) postings.Add(posting);
                }
            }
            if (postings.Count == 0)
            {
                for (int i = 0; i < runes.Length; i++)
                {
                    ulong key = Pack(runes, i, 1);
                    if (index.TryGet(key, out Posting posting)) postings.Add(posting);
                }
            }
            // A token with no retained selective posting is an unconstrained token. A
            // complete scan is conservative and avoids false negatives for high-frequency
            // keys intentionally omitted from the compact index.
            if (postings.Count == 0) continue;
            {
                postings.Sort(static (a, b) => a.Count.CompareTo(b.Count));
                int[] tokenCandidates = postings[0].ToArray();
                for (int i = 1; i < postings.Count && tokenCandidates.Length > 0; i++)
                    tokenCandidates = Intersect(tokenCandidates, postings[i].ToArray());
                intersection = intersection is null ? tokenCandidates : Intersect(intersection, tokenCandidates);
                if (intersection.Length == 0) return [];
            }
        }
        return intersection;
    }

    private static int[] Intersect(int[] left, int[] right)
    {
        int[] buffer = new int[Math.Min(left.Length, right.Length)];
        int i = 0, j = 0, count = 0;
        while (i < left.Length && j < right.Length)
        {
            if (left[i] == right[j]) { buffer[count++] = left[i]; i++; j++; }
            else if (left[i] < right[j]) i++;
            else j++;
        }
        return count == buffer.Length ? buffer : buffer[..count];
    }

    private static ulong Pack(IReadOnlyList<Rune> runes, int start, int length)
    {
        ulong key = 1;
        for (int i = start; i < start + length; i++)
            key = checked((key << 21) | (uint)runes[i].Value);
        return key;
    }

    private static void WriteIndex(BinaryWriter writer, CompactIndex index)
    {
        writer.Write(index.Count);
        for (int i = 0; i < index.Count; i++)
        {
            writer.Write(index.Keys[i]);
            int count = index.Counts[i];
            writer.Write(count);
            int offset = index.Offsets[i];
            int byteCount = index.Offsets[i + 1] - offset;
            writer.Write(byteCount);
            writer.Write(index.Data, offset, byteCount);
        }
    }

    private static CompactIndex ReadIndex(BinaryReader reader)
    {
        int count = reader.ReadInt32();
        if (count < 0 || count > 20_000_000) throw new InvalidDataException("Route C index count invalid");
        var keys = new ulong[count];
        var counts = new int[count];
        var offsets = new int[count + 1];
        var encoded = new ArrayBufferWriter<byte>();
        byte[] transfer = new byte[64 * 1024];
        for (int i = 0; i < count; i++)
        {
            ulong key = reader.ReadUInt64();
            if (i > 0 && key <= keys[i - 1]) throw new InvalidDataException("Route C index keys are not strictly increasing");
            int postingCount = reader.ReadInt32();
            if (postingCount < 0 || postingCount > 20_000_000)
                throw new InvalidDataException("Route C posting header invalid");
            keys[i] = key;
            counts[i] = postingCount;
            int byteCount = reader.ReadInt32();
            if (byteCount < 0 || byteCount > 1_000_000_000)
                throw new InvalidDataException("Route C encoded posting length invalid");
            int remaining = byteCount;
            while (remaining > 0)
            {
                int chunk = Math.Min(remaining, transfer.Length);
                int read = reader.Read(transfer, 0, chunk);
                if (read != chunk) throw new EndOfStreamException();
                transfer.AsSpan(0, read).CopyTo(encoded.GetSpan(read));
                encoded.Advance(read);
                remaining -= read;
            }
            offsets[i + 1] = encoded.WrittenCount;
        }
        return new CompactIndex(keys, offsets, counts, encoded.WrittenSpan.ToArray());
    }

    private static void ValidateIds(IReadOnlyList<FileRecord> source)
    {
        int previous = int.MinValue;
        foreach (FileRecord record in source)
        {
            if (record.FileId <= previous) throw new ArgumentException("FileId values must be unique and sortable");
            previous = record.FileId;
        }
    }

    private static void ValidateSortedIds(IReadOnlyList<int> source)
    {
        int previous = int.MinValue;
        foreach (int id in source)
        {
            if (id <= previous) throw new InvalidDataException("Route C ids are not strictly increasing");
            previous = id;
        }
    }

    private static FileRecord Minimal(int id) => new(id, null, string.Empty, string.Empty, 0, 0, 0);

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);

    private sealed class CandidateRecordList : IReadOnlyList<FileRecord>
    {
        private readonly int[] ids;
        private readonly int[]? candidates;
        private readonly int count;

        public CandidateRecordList(int[] ids, int[]? candidates, int limit)
        {
            this.ids = ids;
            this.candidates = candidates;
            int available = candidates?.Length ?? ids.Length;
            count = limit > 0 ? Math.Min(limit, available) : available;
        }

        public int Count => count;
        public FileRecord this[int index]
        {
            get
            {
                if ((uint)index >= (uint)count) throw new ArgumentOutOfRangeException(nameof(index));
                int baseIndex = candidates is null ? index : candidates[index];
                return Minimal(ids[baseIndex]);
            }
        }

        public IEnumerator<FileRecord> GetEnumerator()
        {
            for (int i = 0; i < count; i++) yield return this[i];
        }
        System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();
    }

    private sealed class CompactIndex
    {
        public static CompactIndex Empty { get; } = new([], [0], [], []);

        public CompactIndex(ulong[] keys, int[] offsets, int[] counts, byte[] data)
        {
            Keys = keys; Offsets = offsets; Counts = counts; Data = data;
        }

        public ulong[] Keys { get; }
        public int[] Offsets { get; }
        public int[] Counts { get; }
        public byte[] Data { get; }
        public int Count => Keys.Length;

        public bool TryGet(ulong key, out Posting posting)
        {
            int low = 0, high = Keys.Length - 1;
            while (low <= high)
            {
                int middle = low + ((high - low) >> 1);
                ulong current = Keys[middle];
                if (current == key)
                {
                    posting = new Posting(Data, Offsets[middle], Offsets[middle + 1] - Offsets[middle], Counts[middle]);
                    return true;
                }
                if (current < key) low = middle + 1; else high = middle - 1;
            }
            posting = default;
            return false;
        }
    }

    private readonly struct Posting
    {
        private readonly byte[] data;
        private readonly int offset;
        private readonly int byteCount;

        public Posting(byte[] data, int offset, int byteCount, int count)
        {
            this.data = data; this.offset = offset; this.byteCount = byteCount; Count = count;
        }

        public int Count { get; }

        public int[] ToArray()
        {
            if (Count == 0) return [];
            var result = new int[Count];
            int cursor = offset;
            int end = checked(offset + byteCount);
            int previous = 0;
            for (int i = 0; i < result.Length; i++)
            {
                uint value = 0;
                int shift = 0;
                while (cursor < end)
                {
                    byte next = data[cursor++];
                    value |= (uint)(next & 0x7F) << shift;
                    if ((next & 0x80) == 0) break;
                    shift += 7;
                    if (shift > 28) throw new InvalidDataException("Route C varint too long");
                }
                if (cursor > end) throw new EndOfStreamException();
                previous = checked(previous + (int)value);
                result[i] = previous;
            }
            if (cursor != end) throw new InvalidDataException("Route C posting has trailing bytes");
            return result;
        }
    }
}

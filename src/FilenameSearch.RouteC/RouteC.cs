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
    private const string Magic = "PRFRC003";
    private const int Version = 3;
    private readonly object gate = new();
    private int[] ids = [];
    private Dictionary<string, Posting> nameIndex = new(StringComparer.Ordinal);
    private Dictionary<string, Posting> pathIndex = new(StringComparer.Ordinal);
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
        for (int i = 0; i < ordered.Length; i++)
        {
            names[i] = FilenameSemantics.Normalize(ordered[i].Name, false);
            paths[i] = FilenameSemantics.Normalize(ordered[i].FullPath, false);
        }

        Dictionary<string, Posting>? nextNames = null;
        Dictionary<string, Posting>? nextPaths = null;
        Parallel.Invoke(
            () => nextNames = BuildIndex(names, includeOneRune: true),
            () => nextPaths = BuildIndex(paths, includeOneRune: true));

        lock (gate)
        {
            ThrowIfDisposed();
            ids = ordered.Select(r => r.FileId).ToArray();
            nameIndex = nextNames!;
            pathIndex = nextPaths!;
            overlay.Clear();
        }
    }

    public void Save(string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string full = Path.GetFullPath(store);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        int[] idSnapshot;
        Dictionary<string, Posting> nameSnapshot;
        Dictionary<string, Posting> pathSnapshot;
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
        Dictionary<string, Posting> nextNames = ReadIndex(reader);
        Dictionary<string, Posting> nextPaths = ReadIndex(reader);
        if (stream.Position != stream.Length) throw new InvalidDataException("Route C store has trailing bytes");
        ValidatePostingBounds(nextNames, count);
        ValidatePostingBounds(nextPaths, count);

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

            Dictionary<string, Posting> index = request.Scope == FilenameScope.Filename ? nameIndex : pathIndex;
            int[]? candidates = CandidateIndexes(tokens, index);
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
            nameIndex.Clear();
            pathIndex.Clear();
            overlay.Clear();
        }
        GC.SuppressFinalize(this);
    }

    private static Dictionary<string, Posting> BuildIndex(IReadOnlyList<string> values, bool includeOneRune)
    {
        // Complete one/two-rune postings are the correctness backbone. Selective trigrams
        // are added only when their document frequency is small enough to be useful.
        var shortLists = new Dictionary<string, List<int>>(StringComparer.Ordinal);
        var trigramCounts = new Dictionary<string, int>(StringComparer.Ordinal);
        const int mediumDivisor = 32;
        int medium = Math.Max(4_096, Math.Max(1, values.Count / mediumDivisor));

        for (int i = 0; i < values.Count; i++)
        {
            Rune[] runes = values[i].EnumerateRunes().ToArray();
            var seenShort = new HashSet<string>(StringComparer.Ordinal);
            if (includeOneRune)
            {
                for (int p = 0; p < runes.Length; p++) seenShort.Add(ToString(runes, p, 1));
            }
            for (int p = 0; p + 2 <= runes.Length; p++) seenShort.Add(ToString(runes, p, 2));
            foreach (string key in seenShort)
            {
                if (!shortLists.TryGetValue(key, out List<int>? list)) shortLists[key] = list = [];
                list.Add(i);
            }

            var seenTri = new HashSet<string>(StringComparer.Ordinal);
            for (int p = 0; p + 3 <= runes.Length; p++) seenTri.Add(ToString(runes, p, 3));
            foreach (string key in seenTri)
            {
                if (trigramCounts.TryGetValue(key, out int count)) trigramCounts[key] = count + 1;
                else trigramCounts[key] = 1;
            }
        }

        var result = new Dictionary<string, Posting>(shortLists.Count + trigramCounts.Count / 2, StringComparer.Ordinal);
        foreach ((string key, List<int> list) in shortLists)
            result[key] = new Posting(list.Count, EncodePosting(list));

        HashSet<string> selective = trigramCounts
            .Where(p => p.Value <= medium)
            .Select(p => p.Key)
            .ToHashSet(StringComparer.Ordinal);
        if (selective.Count != 0)
        {
            var triLists = selective.ToDictionary(k => k, _ => new List<int>(), StringComparer.Ordinal);
            for (int i = 0; i < values.Count; i++)
            {
                Rune[] runes = values[i].EnumerateRunes().ToArray();
                var seen = new HashSet<string>(StringComparer.Ordinal);
                for (int p = 0; p + 3 <= runes.Length; p++)
                {
                    string key = ToString(runes, p, 3);
                    if (selective.Contains(key)) seen.Add(key);
                }
                foreach (string key in seen) triLists[key].Add(i);
            }
            foreach ((string key, List<int> list) in triLists)
                result[key] = new Posting(list.Count, EncodePosting(list));
        }
        return result;
    }

    private static int[]? CandidateIndexes(string[] tokens, IReadOnlyDictionary<string, Posting> index)
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
                    string key = ToString(runes, i, 3);
                    if (index.TryGetValue(key, out Posting? posting)) postings.Add(posting);
                }
            }
            if (postings.Count == 0 && runes.Length >= 2)
            {
                for (int i = 0; i + 2 <= runes.Length; i++)
                {
                    string key = ToString(runes, i, 2);
                    if (!index.TryGetValue(key, out Posting? posting)) return [];
                    postings.Add(posting);
                }
            }
            if (postings.Count == 0)
            {
                string key = ToString(runes, 0, 1);
                if (!index.TryGetValue(key, out Posting? posting)) return [];
                postings.Add(posting);
            }

            postings.Sort(static (a, b) => a.Count.CompareTo(b.Count));
            int[] tokenCandidates = postings[0].ToArray();
            for (int i = 1; i < postings.Count && tokenCandidates.Length > 0; i++)
                tokenCandidates = Intersect(tokenCandidates, postings[i].ToArray());
            intersection = intersection is null ? tokenCandidates : Intersect(intersection, tokenCandidates);
            if (intersection.Length == 0) return [];
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

    private static string ToString(IReadOnlyList<Rune> runes, int start, int length)
    {
        var builder = new StringBuilder(length * 2);
        for (int i = start; i < start + length; i++) builder.Append(runes[i].ToString());
        return builder.ToString();
    }

    private static byte[] EncodePosting(IReadOnlyList<int> values)
    {
        var output = new ArrayBufferWriter<byte>();
        int previous = 0;
        foreach (int value in values)
        {
            uint delta = checked((uint)(value - previous));
            do
            {
                Span<byte> span = output.GetSpan(1);
                byte next = (byte)(delta & 0x7F);
                delta >>= 7;
                if (delta != 0) next |= 0x80;
                span[0] = next;
                output.Advance(1);
            } while (delta != 0);
            previous = value;
        }
        return output.WrittenSpan.ToArray();
    }

    private static uint ReadVarUInt(ReadOnlySpan<byte> data, ref int offset)
    {
        uint value = 0;
        int shift = 0;
        while (offset < data.Length)
        {
            byte b = data[offset++];
            value |= (uint)(b & 0x7F) << shift;
            if ((b & 0x80) == 0) return value;
            shift += 7;
            if (shift > 28) throw new InvalidDataException("Route C varint too long");
        }
        throw new EndOfStreamException();
    }

    private static void WriteIndex(BinaryWriter writer, IReadOnlyDictionary<string, Posting> index)
    {
        writer.Write(index.Count);
        foreach ((string key, Posting posting) in index.OrderBy(p => p.Key, StringComparer.Ordinal))
        {
            writer.Write(key);
            writer.Write(posting.Count);
            writer.Write(posting.Data.Length);
            writer.Write(posting.Data);
        }
    }

    private static Dictionary<string, Posting> ReadIndex(BinaryReader reader)
    {
        int count = reader.ReadInt32();
        if (count < 0 || count > 20_000_000) throw new InvalidDataException("Route C index count invalid");
        var result = new Dictionary<string, Posting>(Math.Min(count, 4_000_000), StringComparer.Ordinal);
        for (int i = 0; i < count; i++)
        {
            string key = reader.ReadString();
            int postingCount = reader.ReadInt32();
            int byteCount = reader.ReadInt32();
            if (postingCount < 0 || byteCount < 0 || byteCount > 1_000_000_000)
                throw new InvalidDataException("Route C posting header invalid");
            byte[] data = reader.ReadBytes(byteCount);
            if (data.Length != byteCount) throw new EndOfStreamException();
            if (!result.TryAdd(key, new Posting(postingCount, data)))
                throw new InvalidDataException("Route C duplicate index key");
        }
        return result;
    }

    private static void ValidatePostingBounds(IReadOnlyDictionary<string, Posting> index, int recordCount)
    {
        foreach (Posting posting in index.Values)
        {
            int[] values = posting.ToArray();
            int previous = -1;
            foreach (int value in values)
            {
                if ((uint)value >= (uint)recordCount || value <= previous)
                    throw new InvalidDataException("Route C posting index is out of bounds or unsorted");
                previous = value;
            }
        }
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

    private sealed class Posting
    {
        public Posting(int count, byte[] data) { Count = count; Data = data; }
        public int Count { get; }
        public byte[] Data { get; }

        public int[] ToArray()
        {
            var result = new int[Count];
            int cursor = 0;
            int previous = 0;
            for (int i = 0; i < result.Length; i++)
            {
                previous = checked(previous + (int)ReadVarUInt(Data, ref cursor));
                result[i] = previous;
            }
            if (cursor != Data.Length) throw new InvalidDataException("Route C posting has trailing bytes");
            return result;
        }
    }
}

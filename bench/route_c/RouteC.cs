using System.Buffers;
using System.Diagnostics;
using System.Text;
using FilenameSearch.Core;

namespace FilenameSearch.RouteC;

/// <summary>Route C: corpus-statistics-driven hybrid of complete short-gram and selective trigram postings.</summary>
public sealed class RouteCEngine : IFilenameSearchEngine
{
    private const string Magic = "FRC002";
    private readonly object gate = new();
    private FileRecord[] records = [];
    private StringTable namesFolded = StringTable.Empty, pathsFolded = StringTable.Empty;
    private Dictionary<string, Posting> nameIndex = new(StringComparer.Ordinal), pathIndex = new(StringComparer.Ordinal);
    private Dictionary<int, Prepared?> changes = [];
    private HashSet<int> ids = [];
    private FileStream? lazyStore;
    private int rareThreshold, mediumThreshold;

    public string RouteName => "C";
    public int RareThreshold { get { lock (gate) return rareThreshold; } }
    public int MediumThreshold { get { lock (gate) return mediumThreshold; } }
    public IReadOnlyList<FileRecord> Records { get { lock (gate) return SnapshotRecords(); } }

    public void Build(IReadOnlyList<FileRecord> source)
    {
        FileRecord[] next = source.OrderBy(r => r.FileId).Select(Canonicalize).ToArray(); ValidateIds(next);
        string[] nf = next.Select(r => FilenameSemantics.Normalize(r.Name, false)).ToArray();
        string[] pf = next.Select(r => FilenameSemantics.Normalize(r.FullPath, false)).ToArray();
        (int rare, int medium) = Thresholds(next.Length);
        Dictionary<string, Posting>? ni = null, pi = null;
        Parallel.Invoke(() => ni = BuildIndex(nf, rare, medium, 1), () => pi = BuildIndex(pf, rare, medium, 2));
        lock (gate) { CloseLazyStore(); records = next; namesFolded = new StringTable(nf); pathsFolded = new StringTable(pf); nameIndex = ni!; pathIndex = pi!; rareThreshold = rare; mediumThreshold = medium; ids = next.Select(r => r.FileId).ToHashSet(); changes = []; }
    }

    public void Save(string store)
    {
        string fullPath = Path.GetFullPath(store); Directory.CreateDirectory(Path.GetDirectoryName(fullPath)!);
        FileRecord[] current; string[] nf, pf;
        lock (gate) { current = SnapshotRecords(); nf = current.Select(r => FilenameSearch.Core.FilenameSemantics.Normalize(r.Name, false)).ToArray(); pf = current.Select(r => FilenameSearch.Core.FilenameSemantics.Normalize(r.FullPath, false)).ToArray(); }
        Dictionary<string, Posting>? ni = null, pi = null;
        Parallel.Invoke(() => ni = BuildIndex(nf, rareThreshold, mediumThreshold, 1), () => pi = BuildIndex(pf, rareThreshold, mediumThreshold, 2));
        using var stream = new FileStream(fullPath, FileMode.Create, FileAccess.Write, FileShare.Read); using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: false);
        writer.Write(Magic); writer.Write(1); writer.Write(current.Length); writer.Write(rareThreshold); writer.Write(mediumThreshold); foreach (FileRecord record in current) WriteRecord(writer, record);
        WriteStrings(writer, nf); WriteStrings(writer, pf); WriteIndex(writer, ni!); WriteIndex(writer, pi!);
    }

    public void Load(string store)
    {
        FileStream stream = new(store, FileMode.Open, FileAccess.Read, FileShare.Read); bool keepOpen = false;
        try
        {
            using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: true);
            if (reader.ReadString() != Magic || reader.ReadInt32() != 1) throw new InvalidDataException("Route C store header mismatch");
            int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route C store count invalid"); int rare = reader.ReadInt32(), medium = reader.ReadInt32();
            var next = new FileRecord[count]; for (int i = 0; i < count; i++) next[i] = Canonicalize(ReadRecord(reader));
            StringTable nf = ReadStringTable(reader, stream, count), pf = ReadStringTable(reader, stream, count); Dictionary<string, Posting> ni = ReadIndex(reader, stream), pi = ReadIndex(reader, stream);
            if (stream.Position != stream.Length) throw new InvalidDataException("Route C store has trailing bytes"); ValidateIds(next);
            lock (gate) { CloseLazyStore(); records = next; namesFolded = nf; pathsFolded = pf; nameIndex = ni; pathIndex = pi; rareThreshold = rare; mediumThreshold = medium; ids = next.Select(r => r.FileId).ToHashSet(); changes = []; lazyStore = stream; keepOpen = true; }
        }
        finally { if (!keepOpen) stream.Dispose(); }
    }

    public SearchResult Search(FilenameQuery request)
    {
        if (request is null) throw new ArgumentNullException(nameof(request)); var stopwatch = Stopwatch.StartNew();
        string[] tokens = request.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries).Select(t => FilenameSemantics.Normalize(t, request.CaseSensitive)).ToArray(); var results = new List<FileRecord>(); int candidates; bool usedScan; bool requiresSort;
        lock (gate)
        {
            requiresSort = changes.Count > 0;
            if (changes.Count > 0) { usedScan = true; candidates = 0; for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) { candidates++; if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) { candidates++; if (Matches(change.Value, request.Scope, request.CaseSensitive, tokens)) results.Add(change.Value.Record); } }
            else { int[]? candidateIndexes = CandidateIndexes(tokens, request.Scope, request.CaseSensitive, out usedScan); candidates = candidateIndexes?.Length ?? records.Length; if (candidateIndexes is null) { for (int index = 0; index < records.Length; index++) { Prepared current = new(records[index], namesFolded[index], pathsFolded[index]); if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } } else foreach (int index in candidateIndexes) { Prepared current = new(records[index], namesFolded[index], pathsFolded[index]); if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } }
        }
        if (requiresSort) results.Sort((a, b) => a.FileId.CompareTo(b.FileId)); if (request.Limit > 0 && results.Count > request.Limit) results.RemoveRange(request.Limit, results.Count - request.Limit); stopwatch.Stop(); return new SearchResult(results, stopwatch.Elapsed.TotalMilliseconds, candidates, usedScan);
    }

    public void Upsert(FileRecord record) { lock (gate) changes[record.FileId] = Prepare(record); }
    public bool Remove(int fileId) { lock (gate) { bool existed = ids.Contains(fileId) || (changes.TryGetValue(fileId, out Prepared? current) && current is not null); if (!existed) return false; changes[fileId] = null; return true; } }

    private int[]? CandidateIndexes(string[] tokens, FilenameScope scope, bool caseSensitive, out bool usedScan)
    {
        Dictionary<string, Posting> index = scope == FilenameScope.Filename ? nameIndex : pathIndex; int[]? intersection = null; bool indexed = false;
        foreach (string token in tokens)
        {
            string indexToken = caseSensitive ? FilenameSemantics.Normalize(token, false) : token; int[]? tokenCandidates = FindTokenCandidates(index, indexToken);
            if (tokenCandidates is null) { usedScan = true; return null; }
            indexed = true; if (tokenCandidates.Length == 0) { usedScan = false; return []; }
            intersection = intersection is null ? tokenCandidates : Intersect(intersection, tokenCandidates); if (intersection.Length == 0) { usedScan = false; return []; }
        }
        if (!indexed) { usedScan = true; return null; } usedScan = false; return intersection!;
    }

    private static int[]? FindTokenCandidates(IReadOnlyDictionary<string, Posting> index, string token)
    {
        for (int length = 3; length >= 1; length--)
        {
            string[] keys = (token.IndexOfAny(['*', '?']) >= 0 ? ExtractPatternNgrams(token, length) : ExtractNgrams(token, length)).Distinct(StringComparer.Ordinal).ToArray();
            if (keys.Length == 0) continue;
            var postings = new List<Posting>(keys.Length);
            foreach (string key in keys) if (index.TryGetValue(key, out Posting? posting) && posting is not null) postings.Add(posting);
            if (postings.Count == 0)
            {
                if (length <= 2) return [];
                continue;
            }
            postings.Sort((left, right) => left.Count.CompareTo(right.Count)); int[] result = postings[0].ToArray();
            for (int i = 1; i < postings.Count && result.Length > 0; i++) result = Intersect(result, postings[i].ToArray());
            return result;
        }
        return null;
    }

    private static Dictionary<string, Posting> BuildIndex(IReadOnlyList<string> values, int rare, int medium, int minimumLength)
    {
        var mutable = new Dictionary<string, List<int>>(StringComparer.Ordinal);
        for (int i = 0; i < values.Count; i++) foreach ((string key, int length) in EnumerateIndexedNgrams(values[i], minimumLength))
        {
            if (!mutable.TryGetValue(key, out List<int>? list)) mutable[key] = list = [];
            if (list.Count == 0 || list[^1] != i)
            {
                // Counts above the common trigram threshold are not persisted, so stop growing those lists.
                if (length >= 3 && list.Count > medium) continue;
                list.Add(i);
            }
        }
        var result = new Dictionary<string, Posting>(mutable.Count, StringComparer.Ordinal);
        foreach ((string key, List<int> list) in mutable)
        {
            int length = key.EnumerateRunes().Count();
            // Short grams are complete so 1/2-code-point searches avoid a full scan. Common trigrams stay scan-backed.
            if (length <= 2 || list.Count <= medium) result.Add(key, new Posting(list.Count, EncodePosting(list)));
        }
        return result;
    }

    private static IEnumerable<(string Key, int Length)> EnumerateIndexedNgrams(string value, int minimumLength)
    {
        Rune[] runes = value.EnumerateRunes().ToArray();
        for (int length = minimumLength; length <= 3; length++) for (int i = 0; i + length <= runes.Length; i++) yield return (ToString(runes, i, length), length);
    }
    private static IEnumerable<string> ExtractNgrams(string value, int length) { Rune[] runes = value.EnumerateRunes().ToArray(); for (int i = 0; i + length <= runes.Length; i++) yield return ToString(runes, i, length); }
    private static IEnumerable<string> ExtractPatternNgrams(string value, int length)
    {
        var runes = new List<Rune>(); foreach (Rune rune in value.EnumerateRunes()) { if (rune.Value is '*' or '?') { foreach (string key in Ngrams(runes, length)) yield return key; runes.Clear(); } else runes.Add(rune); } foreach (string key in Ngrams(runes, length)) yield return key;
        static IEnumerable<string> Ngrams(List<Rune> values, int length) { for (int i = 0; i + length <= values.Count; i++) yield return ToString(values, i, length); }
    }
    private static string ToString(IReadOnlyList<Rune> runes, int start, int length) { var builder = new StringBuilder(); for (int i = start; i < start + length; i++) builder.Append(runes[i].ToString()); return builder.ToString(); }

    private static int[] Intersect(int[] left, int[] right) { int[] result = new int[Math.Min(left.Length, right.Length)]; int i = 0, j = 0, count = 0; while (i < left.Length && j < right.Length) { if (left[i] == right[j]) { result[count++] = left[i]; i++; j++; } else if (left[i] < right[j]) i++; else j++; } return result[..count]; }
    private bool TryCurrent(int index, out Prepared prepared) { int id = records[index].FileId; if (changes.TryGetValue(id, out Prepared? change)) { if (change.HasValue) { prepared = change.Value; return true; } prepared = default; return false; } prepared = new(records[index], namesFolded[index], pathsFolded[index]); return true; }
    private FileRecord[] SnapshotRecords() { var result = new List<FileRecord>(records.Length + changes.Count); for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) result.Add(current.Record); foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) result.Add(change.Value.Record); result.Sort((a, b) => a.FileId.CompareTo(b.FileId)); return result.ToArray(); }
    private static Prepared Prepare(FileRecord record) { FileRecord canonical = Canonicalize(record); return new(canonical, FilenameSemantics.Normalize(canonical.Name, false), FilenameSemantics.Normalize(canonical.FullPath, false)); }
    private static FileRecord Canonicalize(FileRecord record) => record with { Name = record.Name.Normalize(NormalizationForm.FormC), FullPath = record.FullPath.Normalize(NormalizationForm.FormC) };
    private static (int rare, int medium) Thresholds(int count) { int rare = Math.Max(32, count / 1_000); int medium = Math.Max(rare * 4, count / 32); return (rare, medium); }

    private static bool Matches(Prepared prepared, FilenameScope scope, bool caseSensitive, string[] tokens) { ReadOnlySpan<char> target = (caseSensitive ? (scope == FilenameScope.Filename ? prepared.Record.Name : prepared.Record.FullPath) : (scope == FilenameScope.Filename ? prepared.NameFolded : prepared.PathFolded)).AsSpan(); foreach (string token in tokens) { if (token.IndexOfAny(['*', '?']) >= 0 ? !Glob(target, token.AsSpan()) : !target.Contains(token.AsSpan(), StringComparison.Ordinal)) return false; } return true; }
    private static bool Glob(ReadOnlySpan<char> text, ReadOnlySpan<char> pattern)
    {
        if (pattern.IndexOf('?') < 0)
        {
            int firstStar = pattern.IndexOf('*');
            if (firstStar >= 0)
            {
                ReadOnlySpan<char> prefix = pattern[..firstStar], suffix = pattern[(firstStar + 1)..];
                if (suffix.IndexOf('*') < 0)
                {
                    if (prefix.Length == 0 && suffix.Length == 0) return true;
                    if (prefix.Length == 0) return text.Contains(suffix, StringComparison.Ordinal);
                    if (suffix.Length == 0) return text.StartsWith(prefix, StringComparison.Ordinal);
                    return text.Length >= prefix.Length + suffix.Length && text.StartsWith(prefix, StringComparison.Ordinal) && text.EndsWith(suffix, StringComparison.Ordinal);
                }
                bool anchoredStart = prefix.Length > 0, anchoredEnd = !pattern.EndsWith("*", StringComparison.Ordinal);
                string[] segments = pattern.ToString().Split('*', StringSplitOptions.RemoveEmptyEntries);
                int cursor = 0;
                for (int i = 0; i < segments.Length; i++)
                {
                    ReadOnlySpan<char> segment = segments[i].AsSpan();
                    if (i == 0 && anchoredStart) { if (!text.StartsWith(segment, StringComparison.Ordinal)) return false; cursor = segment.Length; continue; }
                    int relative = text[cursor..].IndexOf(segment, StringComparison.Ordinal); if (relative < 0) return false; cursor += relative + segment.Length;
                }
                return !anchoredEnd || (segments.Length > 0 && text.EndsWith(segments[^1], StringComparison.Ordinal));
            }
        }
        int ti = 0, pi = 0, star = -1, starText = -1; while (ti < text.Length) { if (pi < pattern.Length) { OperationStatus ts = Rune.DecodeFromUtf16(text[ti..], out Rune tr, out int tw); OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw); if (ts == OperationStatus.Done && ps == OperationStatus.Done && pr.Value != '*' && pr.Value != '?' && tr == pr) { ti += tw; pi += pw; continue; } if (ps == OperationStatus.Done && pr.Value == '?') { ti += tw; pi += pw; continue; } if (ps == OperationStatus.Done && pr.Value == '*') { star = pi; starText = ti; pi += pw; continue; } } if (star < 0) return false; OperationStatus ss = Rune.DecodeFromUtf16(text[starText..], out _, out int sw); if (ss != OperationStatus.Done) return false; starText += sw; ti = starText; pi = star + 1; } while (pi < pattern.Length) { OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw); if (ps != OperationStatus.Done || pr.Value != '*') return false; pi += pw; } return true;
    }

    private static void ValidateIds(IReadOnlyList<FileRecord> source) { if (source.Select(r => r.FileId).Distinct().Count() != source.Count) throw new ArgumentException("FileId values must be unique"); }
    private static void WriteRecord(BinaryWriter writer, FileRecord r) { writer.Write(r.FileId); writer.Write(r.ParentId ?? 0); writer.Write(r.ParentId.HasValue); writer.Write(r.SizeBytes); writer.Write(r.ModifiedUtcTicks); writer.Write(r.Flags); writer.Write(r.Name); writer.Write(r.FullPath); }
    private static FileRecord ReadRecord(BinaryReader reader) { int id = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean(); ulong size = reader.ReadUInt64(); long modified = reader.ReadInt64(); byte flags = reader.ReadByte(); return new FileRecord(id, hasParent ? parent : null, reader.ReadString(), reader.ReadString(), size, modified, flags); }
    private static void WriteStrings(BinaryWriter writer, IReadOnlyList<string> values) { writer.Write(values.Count); foreach (string value in values) writer.Write(value); }
    private static StringTable ReadStringTable(BinaryReader reader, FileStream stream, int expected)
    {
        int count = reader.ReadInt32(); if (count != expected) throw new InvalidDataException("Route C string count mismatch");
        var offsets = new long[count]; var lengths = new int[count];
        for (int i = 0; i < count; i++) { int byteCount = Read7BitByteCount(stream); offsets[i] = stream.Position; lengths[i] = byteCount; stream.Seek(byteCount, SeekOrigin.Current); }
        return new StringTable(stream, offsets, lengths);
    }
    private static int Read7BitByteCount(Stream stream)
    {
        int value = 0, shift = 0;
        while (true) { int next = stream.ReadByte(); if (next < 0) throw new EndOfStreamException(); value |= (next & 0x7F) << shift; if ((next & 0x80) == 0) return value; shift += 7; if (shift > 28) throw new InvalidDataException("Route C string length is invalid"); }
    }
    private static void WriteIndex(BinaryWriter writer, IReadOnlyDictionary<string, Posting> index) { writer.Write(index.Count); foreach ((string key, Posting posting) in index.OrderBy(p => p.Key, StringComparer.Ordinal)) { writer.Write(key); writer.Write(posting.Count); writer.Write(posting.Data.Length); writer.Write(posting.Data); } }
    private static Dictionary<string, Posting> ReadIndex(BinaryReader reader, FileStream stream) { int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route C index count invalid"); var result = new Dictionary<string, Posting>(count, StringComparer.Ordinal); for (int i = 0; i < count; i++) { string key = reader.ReadString(); int n = reader.ReadInt32(), byteCount = reader.ReadInt32(); if (n < 0 || byteCount < 0 || byteCount > 1_000_000_000) throw new InvalidDataException("Route C posting is invalid"); long offset = stream.Position; stream.Seek(byteCount, SeekOrigin.Current); result.Add(key, new Posting(n, stream, offset, byteCount)); } return result; }
    private static byte[] EncodePosting(IReadOnlyList<int> values) { var buffer = new ArrayBufferWriter<byte>(); int previous = 0; foreach (int value in values) { uint delta = checked((uint)(value - previous)); while (delta >= 0x80) { Span<byte> span = buffer.GetSpan(1); span[0] = (byte)((delta & 0x7F) | 0x80); buffer.Advance(1); delta >>= 7; } Span<byte> last = buffer.GetSpan(1); last[0] = (byte)delta; buffer.Advance(1); previous = value; } return buffer.WrittenMemory.ToArray(); }
    private static uint ReadVarUInt(ReadOnlySpan<byte> data, ref int offset) { uint value = 0; int shift = 0; while (offset < data.Length) { byte b = data[offset++]; value |= (uint)(b & 0x7F) << shift; if ((b & 0x80) == 0) return value; shift += 7; if (shift > 28) throw new InvalidDataException("Route C varint is too long"); } throw new EndOfStreamException(); }
    private sealed class StringTable : IReadOnlyList<string>
    {
        private readonly string[]? values; private readonly FileStream? stream; private readonly long[]? offsets; private readonly int[]? lengths; private readonly string?[] cache;
        public static StringTable Empty { get; } = new([]);
        public StringTable(string[] values) { this.values = values; cache = values.Cast<string?>().ToArray(); }
        public StringTable(FileStream stream, long[] offsets, int[] lengths) { this.stream = stream; this.offsets = offsets; this.lengths = lengths; cache = new string?[offsets.Length]; }
        public int Count => values?.Length ?? cache.Length;
        public string this[int index]
        {
            get
            {
                if (values is not null) return values[index];
                string? current = Volatile.Read(ref cache[index]); if (current is not null) return current;
                lock (stream!)
                {
                    current = cache[index]; if (current is null) { stream.Position = offsets![index]; byte[] bytes = new byte[lengths![index]]; int read = 0; while (read < bytes.Length) { int n = stream.Read(bytes, read, bytes.Length - read); if (n == 0) throw new EndOfStreamException(); read += n; } current = Encoding.UTF8.GetString(bytes); cache[index] = current; }
                }
                return current;
            }
        }
        public IEnumerator<string> GetEnumerator() { for (int i = 0; i < Count; i++) yield return this[i]; }
        System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();
    }

    private sealed class Posting
    {
        private readonly FileStream? stream; private readonly long offset; private readonly int byteCount; private byte[]? data;
        public Posting(int count, byte[] data) { Count = count; this.data = data; }
        public Posting(int count, FileStream stream, long offset, int byteCount) { Count = count; this.stream = stream; this.offset = offset; this.byteCount = byteCount; }
        public int Count { get; }
        public byte[] Data
        {
            get
            {
                if (data is not null) return data;
                lock (stream!) { stream.Position = offset; data = new byte[byteCount]; int read = 0; while (read < data.Length) { int n = stream.Read(data, read, data.Length - read); if (n == 0) throw new EndOfStreamException(); read += n; } }
                return data;
            }
        }
        public int[] ToArray() { byte[] bytes = Data; var values = new int[Count]; int previous = 0, cursor = 0; for (int i = 0; i < values.Length; i++) { previous = checked(previous + (int)ReadVarUInt(bytes, ref cursor)); values[i] = previous; } if (cursor != bytes.Length) throw new InvalidDataException("Route C posting has trailing bytes"); return values; }
    }
    private void CloseLazyStore() { lazyStore?.Dispose(); lazyStore = null; }
    private readonly record struct Prepared(FileRecord Record, string NameFolded, string PathFolded);
}

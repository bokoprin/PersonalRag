using System.Buffers;
using System.Diagnostics;
using System.Text;
using FilenameSearch.Core;

namespace FilenameSearch.RouteB;

/// <summary>Route B: compact folded trigram postings with scan fallback for short queries.</summary>
public sealed class RouteBEngine : IFilenameSearchEngine
{
    private const string Magic = "FRB002";
    private readonly object gate = new();
    private FileRecord[] records = [];
    private string[] namesFolded = [], pathsFolded = [];
    private Dictionary<string, Posting> namePostings = new(StringComparer.Ordinal), pathPostings = new(StringComparer.Ordinal);
    private Dictionary<int, Prepared?> changes = [];
    private HashSet<int> ids = [];

    public string RouteName => "B";
    public IReadOnlyList<FileRecord> Records { get { lock (gate) return SnapshotRecords(); } }

    public void Build(IReadOnlyList<FileRecord> source)
    {
        FileRecord[] next = source.OrderBy(r => r.FileId).Select(Canonicalize).ToArray(); ValidateIds(next);
        string[] nextNameFolded = next.Select(r => FilenameSemantics.Normalize(r.Name, false)).ToArray();
        string[] nextPathFolded = next.Select(r => FilenameSemantics.Normalize(r.FullPath, false)).ToArray();
        Dictionary<string, Posting> nextNamePostings = BuildPostings(nextNameFolded);
        Dictionary<string, Posting> nextPathPostings = BuildPostings(nextPathFolded);
        lock (gate)
        {
            records = next; namesFolded = nextNameFolded; pathsFolded = nextPathFolded;
            namePostings = nextNamePostings; pathPostings = nextPathPostings; ids = next.Select(r => r.FileId).ToHashSet(); changes = [];
        }
    }

    public void Save(string store)
    {
        string fullPath = Path.GetFullPath(store); Directory.CreateDirectory(Path.GetDirectoryName(fullPath)!);
        FileRecord[] current; string[] nf, pf; Dictionary<string, Posting> ni, pi;
        lock (gate)
        {
            current = SnapshotRecords(); nf = current.Select(r => FilenameSemantics.Normalize(r.Name, false)).ToArray();
            pf = current.Select(r => FilenameSemantics.Normalize(r.FullPath, false)).ToArray(); ni = BuildPostings(nf); pi = BuildPostings(pf);
        }
        using var stream = new FileStream(fullPath, FileMode.Create, FileAccess.Write, FileShare.Read);
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: false);
        writer.Write(Magic); writer.Write(1); writer.Write(current.Length); foreach (FileRecord record in current) WriteRecord(writer, record);
        WriteStrings(writer, nf); WriteStrings(writer, pf); WriteIndex(writer, ni); WriteIndex(writer, pi);
    }

    public void Load(string store)
    {
        using var stream = new FileStream(store, FileMode.Open, FileAccess.Read, FileShare.Read);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: false);
        if (reader.ReadString() != Magic || reader.ReadInt32() != 1) throw new InvalidDataException("Route B store header mismatch");
        int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route B store count invalid");
        var next = new FileRecord[count]; for (int i = 0; i < count; i++) next[i] = Canonicalize(ReadRecord(reader));
        string[] nf = ReadStrings(reader, count), pf = ReadStrings(reader, count); Dictionary<string, Posting> ni = ReadIndex(reader), pi = ReadIndex(reader);
        if (stream.Position != stream.Length) throw new InvalidDataException("Route B store has trailing bytes"); ValidateIds(next);
        lock (gate) { records = next; namesFolded = nf; pathsFolded = pf; namePostings = ni; pathPostings = pi; ids = next.Select(r => r.FileId).ToHashSet(); changes = []; }
    }

    public SearchResult Search(FilenameQuery request)
    {
        if (request is null) throw new ArgumentNullException(nameof(request));
        var stopwatch = Stopwatch.StartNew();
        string[] tokens = request.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries).Select(t => FilenameSemantics.Normalize(t, request.CaseSensitive)).ToArray();
        var results = new List<FileRecord>(); int candidates; bool usedScan; bool requiresSort;
        lock (gate)
        {
            requiresSort = changes.Count > 0;
            // The overlay is deliberately scan-verified until its next persisted rebuild.
            if (changes.Count > 0) { candidates = 0; usedScan = true; for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) { candidates++; if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) { candidates++; if (Matches(change.Value, request.Scope, request.CaseSensitive, tokens)) results.Add(change.Value.Record); } }
            else
            {
                int[]? candidateIndexes = CandidateIndexes(tokens, request.Scope, request.CaseSensitive, out usedScan);
                candidates = candidateIndexes?.Length ?? records.Length;
                if (candidateIndexes is null)
                {
                    for (int index = 0; index < records.Length; index++) if (Matches(new Prepared(records[index], namesFolded[index], pathsFolded[index]), request.Scope, request.CaseSensitive, tokens)) results.Add(records[index]);
                }
                else foreach (int index in candidateIndexes) if (Matches(new Prepared(records[index], namesFolded[index], pathsFolded[index]), request.Scope, request.CaseSensitive, tokens)) results.Add(records[index]);
            }
        }
        if (requiresSort) results.Sort((a, b) => a.FileId.CompareTo(b.FileId));
        if (request.Limit > 0 && results.Count > request.Limit) results.RemoveRange(request.Limit, results.Count - request.Limit);
        stopwatch.Stop(); return new SearchResult(results, stopwatch.Elapsed.TotalMilliseconds, candidates, usedScan);
    }

    public void Upsert(FileRecord record) { lock (gate) changes[record.FileId] = Prepare(record); }
    public bool Remove(int fileId) { lock (gate) { bool existed = ids.Contains(fileId) || (changes.TryGetValue(fileId, out Prepared? current) && current is not null); if (!existed) return false; changes[fileId] = null; return true; } }

    private int[]? CandidateIndexes(string[] tokens, FilenameScope scope, bool caseSensitive, out bool usedScan)
    {
        if (caseSensitive) { usedScan = true; return null; }
        Dictionary<string, Posting> index = scope == FilenameScope.Filename ? namePostings : pathPostings;
        int[]? intersection = null; bool indexed = false;
        foreach (string token in tokens)
        {
            string[] trigrams = (token.IndexOfAny(['*', '?']) >= 0 ? ExtractPatternTrigrams(token) : ExtractTrigrams(token)).Distinct(StringComparer.Ordinal).ToArray();
            if (trigrams.Length == 0) continue;
            indexed = true;
            var postings = new List<Posting>(trigrams.Length);
            foreach (string trigram in trigrams)
            {
                if (!index.TryGetValue(trigram, out Posting posting)) { usedScan = false; return []; }
                postings.Add(posting);
            }
            postings.Sort((left, right) => left.Count.CompareTo(right.Count));
            int[] tokenCandidates = postings[0].ToArray();
            for (int i = 1; i < postings.Count && tokenCandidates.Length > 0; i++) tokenCandidates = Intersect(tokenCandidates, postings[i].ToArray());
            if (tokenCandidates.Length == 0) { usedScan = false; return []; }
            intersection = intersection is null ? tokenCandidates : Intersect(intersection, tokenCandidates);
            if (intersection.Length == 0) { usedScan = false; return []; }
        }
        if (!indexed) { usedScan = true; return null; }
        usedScan = false; return intersection!;
    }

    private static bool Matches(Prepared prepared, FilenameScope scope, bool caseSensitive, string[] tokens)
    {
        ReadOnlySpan<char> target = (caseSensitive ? (scope == FilenameScope.Filename ? prepared.Record.Name : prepared.Record.FullPath) : (scope == FilenameScope.Filename ? prepared.NameFolded : prepared.PathFolded)).AsSpan();
        foreach (string token in tokens) { if (token.IndexOfAny(['*', '?']) >= 0 ? !Glob(target, token.AsSpan()) : !target.Contains(token.AsSpan(), StringComparison.Ordinal)) return false; }
        return true;
    }

    private static IEnumerable<string> ExtractTrigrams(string value)
    {
        Rune[] runes = value.EnumerateRunes().ToArray();
        for (int i = 0; i + 2 < runes.Length; i++) yield return string.Concat(runes[i].ToString(), runes[i + 1].ToString(), runes[i + 2].ToString());
    }

    private static IEnumerable<string> ExtractPatternTrigrams(string value)
    {
        var runes = new List<Rune>();
        foreach (Rune rune in value.EnumerateRunes())
        {
            if (rune.Value is '*' or '?') { foreach (string trigram in Trigrams(runes)) yield return trigram; runes.Clear(); }
            else runes.Add(rune);
        }
        foreach (string trigram in Trigrams(runes)) yield return trigram;
        static IEnumerable<string> Trigrams(List<Rune> values) { for (int i = 0; i + 2 < values.Count; i++) yield return string.Concat(values[i].ToString(), values[i + 1].ToString(), values[i + 2].ToString()); }
    }

    private static Dictionary<string, Posting> BuildPostings(IReadOnlyList<string> values)
    {
        var mutable = new Dictionary<string, List<int>>(StringComparer.Ordinal);
        for (int i = 0; i < values.Count; i++) foreach (string trigram in ExtractTrigrams(values[i])) { if (!mutable.TryGetValue(trigram, out List<int>? list)) mutable[trigram] = list = []; if (list.Count == 0 || list[^1] != i) list.Add(i); }
        var result = new Dictionary<string, Posting>(mutable.Count, StringComparer.Ordinal);
        foreach ((string key, List<int> valuesForKey) in mutable) result.Add(key, new Posting(valuesForKey.Count, EncodePosting(valuesForKey)));
        return result;
    }

    private static byte[] EncodePosting(IReadOnlyList<int> values)
    {
        var buffer = new ArrayBufferWriter<byte>(); int previous = 0;
        foreach (int value in values) { WriteVarUInt(buffer, checked((uint)(value - previous))); previous = value; }
        return buffer.WrittenMemory.ToArray();
    }

    private static int[] Intersect(int[] left, int[] right)
    {
        int[] result = new int[Math.Min(left.Length, right.Length)]; int i = 0, j = 0, count = 0;
        while (i < left.Length && j < right.Length) { if (left[i] == right[j]) { result[count++] = left[i]; i++; j++; } else if (left[i] < right[j]) i++; else j++; }
        return result[..count];
    }

    private bool TryCurrent(int index, out Prepared prepared) { int id = records[index].FileId; if (changes.TryGetValue(id, out Prepared? change)) { if (change.HasValue) { prepared = change.Value; return true; } prepared = default; return false; } prepared = new Prepared(records[index], namesFolded[index], pathsFolded[index]); return true; }
    private FileRecord[] SnapshotRecords() { var result = new List<FileRecord>(records.Length + changes.Count); for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) result.Add(current.Record); foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) result.Add(change.Value.Record); result.Sort((a, b) => a.FileId.CompareTo(b.FileId)); return result.ToArray(); }
    private static Prepared Prepare(FileRecord record) { FileRecord canonical = Canonicalize(record); return new(canonical, FilenameSemantics.Normalize(canonical.Name, false), FilenameSemantics.Normalize(canonical.FullPath, false)); }
    private static FileRecord Canonicalize(FileRecord record) => record with { Name = record.Name.Normalize(NormalizationForm.FormC), FullPath = record.FullPath.Normalize(NormalizationForm.FormC) };

    private static bool Glob(ReadOnlySpan<char> text, ReadOnlySpan<char> pattern)
    {
        int ti = 0, pi = 0, star = -1, starText = -1;
        while (ti < text.Length)
        {
            if (pi < pattern.Length)
            {
                OperationStatus ts = Rune.DecodeFromUtf16(text[ti..], out Rune tr, out int tw); OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw);
                if (ts == OperationStatus.Done && ps == OperationStatus.Done && pr.Value != '*' && pr.Value != '?' && tr == pr) { ti += tw; pi += pw; continue; }
                if (ps == OperationStatus.Done && pr.Value == '?') { ti += tw; pi += pw; continue; }
                if (ps == OperationStatus.Done && pr.Value == '*') { star = pi; starText = ti; pi += pw; continue; }
            }
            if (star < 0) return false; OperationStatus ss = Rune.DecodeFromUtf16(text[starText..], out _, out int sw); if (ss != OperationStatus.Done) return false; starText += sw; ti = starText; pi = star + 1;
        }
        while (pi < pattern.Length) { OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw); if (ps != OperationStatus.Done || pr.Value != '*') return false; pi += pw; }
        return true;
    }

    private static void ValidateIds(IReadOnlyList<FileRecord> source) { if (source.Select(r => r.FileId).Distinct().Count() != source.Count) throw new ArgumentException("FileId values must be unique"); }
    private static void WriteRecord(BinaryWriter writer, FileRecord r) { writer.Write(r.FileId); writer.Write(r.ParentId ?? 0); writer.Write(r.ParentId.HasValue); writer.Write(r.SizeBytes); writer.Write(r.ModifiedUtcTicks); writer.Write(r.Flags); writer.Write(r.Name); writer.Write(r.FullPath); }
    private static FileRecord ReadRecord(BinaryReader reader) { int id = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean(); ulong size = reader.ReadUInt64(); long modified = reader.ReadInt64(); byte flags = reader.ReadByte(); return new FileRecord(id, hasParent ? parent : null, reader.ReadString(), reader.ReadString(), size, modified, flags); }
    private static void WriteStrings(BinaryWriter writer, IReadOnlyList<string> values) { writer.Write(values.Count); foreach (string value in values) writer.Write(value); }
    private static string[] ReadStrings(BinaryReader reader, int expected) { int count = reader.ReadInt32(); if (count != expected) throw new InvalidDataException("Route B string count mismatch"); var values = new string[count]; for (int i = 0; i < count; i++) values[i] = reader.ReadString(); return values; }
    private static void WriteIndex(BinaryWriter writer, IReadOnlyDictionary<string, Posting> index) { writer.Write(index.Count); foreach ((string key, Posting posting) in index.OrderBy(p => p.Key, StringComparer.Ordinal)) { writer.Write(key); writer.Write(posting.Count); writer.Write(posting.Data.Length); writer.Write(posting.Data); } }
    private static Dictionary<string, Posting> ReadIndex(BinaryReader reader) { int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route B index count invalid"); var index = new Dictionary<string, Posting>(count, StringComparer.Ordinal); for (int i = 0; i < count; i++) { string key = reader.ReadString(); int n = reader.ReadInt32(), byteCount = reader.ReadInt32(); if (n < 0 || byteCount < 0 || byteCount > 1_000_000_000) throw new InvalidDataException("Route B posting is invalid"); byte[] data = reader.ReadBytes(byteCount); if (data.Length != byteCount) throw new EndOfStreamException(); index.Add(key, new Posting(n, data)); } return index; }
    private static void WriteVarUInt(ArrayBufferWriter<byte> buffer, uint value) { while (value >= 0x80) { Span<byte> span = buffer.GetSpan(1); span[0] = (byte)((value & 0x7F) | 0x80); buffer.Advance(1); value >>= 7; } Span<byte> last = buffer.GetSpan(1); last[0] = (byte)value; buffer.Advance(1); }
    private static uint ReadVarUInt(ReadOnlySpan<byte> data, ref int offset) { uint value = 0; int shift = 0; while (offset < data.Length) { byte b = data[offset++]; value |= (uint)(b & 0x7F) << shift; if ((b & 0x80) == 0) return value; shift += 7; if (shift > 28) throw new InvalidDataException("Route B varint is too long"); } throw new EndOfStreamException(); }

    private readonly record struct Posting(int Count, byte[] Data)
    {
        public int[] ToArray()
        {
            var values = new int[Count]; int previous = 0, offset = 0;
            for (int i = 0; i < values.Length; i++) { previous = checked(previous + (int)ReadVarUInt(Data, ref offset)); values[i] = previous; }
            if (offset != Data.Length) throw new InvalidDataException("Route B posting has trailing bytes");
            return values;
        }
    }

    private readonly record struct Prepared(FileRecord Record, string NameFolded, string PathFolded);
}

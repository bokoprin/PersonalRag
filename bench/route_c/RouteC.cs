using System.Buffers;
using System.Diagnostics;
using System.Numerics;
using System.Text;
using FilenameSearch.Core;

namespace FilenameSearch.RouteC;

/// <summary>Route C: corpus-statistics-driven hybrid of compact lists, bitmaps, and scan fallback.</summary>
public sealed class RouteCEngine : IFilenameSearchEngine
{
    private const string Magic = "FRC001";
    private readonly object gate = new();
    private FileRecord[] records = [];
    private string[] namesSensitive = [], pathsSensitive = [], namesFolded = [], pathsFolded = [];
    private Dictionary<string, Posting> nameIndex = new(StringComparer.Ordinal), pathIndex = new(StringComparer.Ordinal);
    private Dictionary<int, Prepared?> changes = [];
    private HashSet<int> ids = [];
    private int rareThreshold, mediumThreshold;

    public string RouteName => "C";
    public int RareThreshold { get { lock (gate) return rareThreshold; } }
    public int MediumThreshold { get { lock (gate) return mediumThreshold; } }
    public IReadOnlyList<FileRecord> Records { get { lock (gate) return SnapshotRecords(); } }

    public void Build(IReadOnlyList<FileRecord> source)
    {
        FileRecord[] next = source.OrderBy(r => r.FileId).ToArray(); ValidateIds(next);
        string[] ns = next.Select(r => r.Name.Normalize(NormalizationForm.FormC)).ToArray();
        string[] ps = next.Select(r => r.FullPath.Normalize(NormalizationForm.FormC)).ToArray();
        string[] nf = next.Select(r => FilenameSemantics.Normalize(r.Name, false)).ToArray();
        string[] pf = next.Select(r => FilenameSemantics.Normalize(r.FullPath, false)).ToArray();
        (int rare, int medium) = Thresholds(next.Length);
        Dictionary<string, Posting> ni = BuildIndex(nf, next.Length, rare, medium);
        Dictionary<string, Posting> pi = BuildIndex(pf, next.Length, rare, medium);
        lock (gate) { records = next; namesSensitive = ns; pathsSensitive = ps; namesFolded = nf; pathsFolded = pf; nameIndex = ni; pathIndex = pi; rareThreshold = rare; mediumThreshold = medium; ids = next.Select(r => r.FileId).ToHashSet(); changes = []; }
    }

    public void Save(string store)
    {
        string fullPath = Path.GetFullPath(store); Directory.CreateDirectory(Path.GetDirectoryName(fullPath)!);
        FileRecord[] current; string[] ns, ps, nf, pf; Dictionary<string, Posting> ni, pi; int rare, medium;
        lock (gate) { current = SnapshotRecords(); ns = current.Select(r => r.Name.Normalize(NormalizationForm.FormC)).ToArray(); ps = current.Select(r => r.FullPath.Normalize(NormalizationForm.FormC)).ToArray(); nf = current.Select(r => FilenameSemantics.Normalize(r.Name, false)).ToArray(); pf = current.Select(r => FilenameSemantics.Normalize(r.FullPath, false)).ToArray(); (rare, medium) = Thresholds(current.Length); ni = BuildIndex(nf, current.Length, rare, medium); pi = BuildIndex(pf, current.Length, rare, medium); }
        using var stream = new FileStream(fullPath, FileMode.Create, FileAccess.Write, FileShare.Read); using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: false);
        writer.Write(Magic); writer.Write(1); writer.Write(current.Length); writer.Write(rare); writer.Write(medium); foreach (FileRecord record in current) WriteRecord(writer, record);
        WriteStrings(writer, ns); WriteStrings(writer, ps); WriteStrings(writer, nf); WriteStrings(writer, pf); WriteIndex(writer, ni); WriteIndex(writer, pi);
    }

    public void Load(string store)
    {
        using var stream = new FileStream(store, FileMode.Open, FileAccess.Read, FileShare.Read); using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: false);
        if (reader.ReadString() != Magic || reader.ReadInt32() != 1) throw new InvalidDataException("Route C store header mismatch");
        int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route C store count invalid"); int rare = reader.ReadInt32(), medium = reader.ReadInt32();
        var next = new FileRecord[count]; for (int i = 0; i < count; i++) next[i] = ReadRecord(reader);
        string[] ns = ReadStrings(reader, count), ps = ReadStrings(reader, count), nf = ReadStrings(reader, count), pf = ReadStrings(reader, count); Dictionary<string, Posting> ni = ReadIndex(reader, count), pi = ReadIndex(reader, count);
        if (stream.Position != stream.Length) throw new InvalidDataException("Route C store has trailing bytes"); ValidateIds(next);
        lock (gate) { records = next; namesSensitive = ns; pathsSensitive = ps; namesFolded = nf; pathsFolded = pf; nameIndex = ni; pathIndex = pi; rareThreshold = rare; mediumThreshold = medium; ids = next.Select(r => r.FileId).ToHashSet(); changes = []; }
    }

    public SearchResult Search(FilenameQuery request)
    {
        if (request is null) throw new ArgumentNullException(nameof(request)); var stopwatch = Stopwatch.StartNew();
        string[] tokens = request.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries).Select(t => FilenameSemantics.Normalize(t, request.CaseSensitive)).ToArray(); var results = new List<FileRecord>(); int candidates; bool usedScan;
        lock (gate)
        {
            if (changes.Count > 0) { usedScan = true; candidates = 0; for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) { candidates++; if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) { candidates++; if (Matches(change.Value, request.Scope, request.CaseSensitive, tokens)) results.Add(change.Value.Record); } }
            else { int[]? candidateIndexes = CandidateIndexes(tokens, request.Scope, request.CaseSensitive, out usedScan); candidates = candidateIndexes?.Length ?? records.Length; if (candidateIndexes is null) { for (int index = 0; index < records.Length; index++) { Prepared current = new(records[index], namesSensitive[index], pathsSensitive[index], namesFolded[index], pathsFolded[index]); if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } } else foreach (int index in candidateIndexes) { Prepared current = new(records[index], namesSensitive[index], pathsSensitive[index], namesFolded[index], pathsFolded[index]); if (Matches(current, request.Scope, request.CaseSensitive, tokens)) results.Add(current.Record); } }
        }
        results.Sort((a, b) => a.FileId.CompareTo(b.FileId)); if (request.Limit > 0 && results.Count > request.Limit) results.RemoveRange(request.Limit, results.Count - request.Limit); stopwatch.Stop(); return new SearchResult(results, stopwatch.Elapsed.TotalMilliseconds, candidates, usedScan);
    }

    public void Upsert(FileRecord record) { lock (gate) changes[record.FileId] = Prepare(record); }
    public bool Remove(int fileId) { lock (gate) { bool existed = ids.Contains(fileId) || (changes.TryGetValue(fileId, out Prepared? current) && current is not null); if (!existed) return false; changes[fileId] = null; return true; } }

    private int[]? CandidateIndexes(string[] tokens, FilenameScope scope, bool caseSensitive, out bool usedScan)
    {
        if (caseSensitive) { usedScan = true; return null; }
        Dictionary<string, Posting> index = scope == FilenameScope.Filename ? nameIndex : pathIndex; int[]? intersection = null; bool indexed = false;
        foreach (string token in tokens)
        {
            IEnumerable<string> keys = token.IndexOfAny(['*', '?']) >= 0 ? ExtractPatternTrigrams(token) : ExtractTrigrams(token);
            int[]? tokenCandidates = null;
            foreach (string key in keys.Distinct(StringComparer.Ordinal))
            {
                if (!index.TryGetValue(key, out Posting? posting)) continue; indexed = true; int[] values = posting.ToArray(); tokenCandidates = tokenCandidates is null ? values : Intersect(tokenCandidates, values); if (tokenCandidates.Length == 0) { usedScan = false; return []; }
            }
            if (tokenCandidates is null) continue;
            intersection = intersection is null ? tokenCandidates : Intersect(intersection, tokenCandidates); if (intersection.Length == 0) { usedScan = false; return []; }
        }
        if (!indexed || intersection is null || intersection.Length > records.Length / 2) { usedScan = true; return null; }
        usedScan = false; return intersection;
    }

    private static Dictionary<string, Posting> BuildIndex(IReadOnlyList<string> values, int universe, int rare, int medium)
    {
        var mutable = new Dictionary<string, List<int>>(StringComparer.Ordinal);
        for (int i = 0; i < values.Count; i++) foreach (string key in ExtractTrigrams(values[i])) { if (!mutable.TryGetValue(key, out List<int>? list)) mutable[key] = list = []; if (list.Count == 0 || list[^1] != i) list.Add(i); }
        int bitmapCap = BitmapCap(universe);
        HashSet<string> bitmapKeys = mutable.Where(pair => pair.Value.Count > rare && pair.Value.Count <= medium)
            .OrderByDescending(pair => pair.Value.Count).Take(bitmapCap).Select(pair => pair.Key).ToHashSet(StringComparer.Ordinal);
        var result = new Dictionary<string, Posting>(StringComparer.Ordinal);
        foreach ((string key, List<int> valuesForKey) in mutable)
        {
            if (valuesForKey.Count <= rare) result.Add(key, Posting.List(valuesForKey.ToArray()));
            else if (bitmapKeys.Contains(key)) result.Add(key, Posting.Bitmap(valuesForKey, universe));
        }
        return result;
    }

    private static (int rare, int medium) Thresholds(int count) { int rare = Math.Max(32, count / 1_000); int medium = Math.Max(rare * 4, count / 8); return (rare, medium); }
    private static int BitmapCap(int universe) => Math.Max(64, Math.Min(1_024, universe / 1_000));
    private static int[] Intersect(int[] left, int[] right) { int[] result = new int[Math.Min(left.Length, right.Length)]; int i = 0, j = 0, count = 0; while (i < left.Length && j < right.Length) { if (left[i] == right[j]) { result[count++] = left[i]; i++; j++; } else if (left[i] < right[j]) i++; else j++; } return result[..count]; }
    private static IEnumerable<string> ExtractTrigrams(string value) { Rune[] runes = value.EnumerateRunes().ToArray(); for (int i = 0; i + 2 < runes.Length; i++) yield return string.Concat(runes[i].ToString(), runes[i + 1].ToString(), runes[i + 2].ToString()); }
    private static IEnumerable<string> ExtractPatternTrigrams(string value) { var runes = new List<Rune>(); foreach (Rune rune in value.EnumerateRunes()) { if (rune.Value is '*' or '?') { foreach (string key in Trigrams(runes)) yield return key; runes.Clear(); } else runes.Add(rune); } foreach (string key in Trigrams(runes)) yield return key; static IEnumerable<string> Trigrams(List<Rune> values) { for (int i = 0; i + 2 < values.Count; i++) yield return string.Concat(values[i].ToString(), values[i + 1].ToString(), values[i + 2].ToString()); } }
    private static Prepared Prepare(FileRecord record) => new(record, record.Name.Normalize(NormalizationForm.FormC), record.FullPath.Normalize(NormalizationForm.FormC), FilenameSemantics.Normalize(record.Name, false), FilenameSemantics.Normalize(record.FullPath, false));
    private bool TryCurrent(int index, out Prepared prepared) { int id = records[index].FileId; if (changes.TryGetValue(id, out Prepared? change)) { if (change.HasValue) { prepared = change.Value; return true; } prepared = default; return false; } prepared = new(records[index], namesSensitive[index], pathsSensitive[index], namesFolded[index], pathsFolded[index]); return true; }
    private FileRecord[] SnapshotRecords() { var result = new List<FileRecord>(records.Length + changes.Count); for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared current)) result.Add(current.Record); foreach ((int id, Prepared? change) in changes) if (!ids.Contains(id) && change is not null) result.Add(change.Value.Record); result.Sort((a, b) => a.FileId.CompareTo(b.FileId)); return result.ToArray(); }

    private static bool Matches(Prepared prepared, FilenameScope scope, bool caseSensitive, string[] tokens) { ReadOnlySpan<char> target = (caseSensitive ? (scope == FilenameScope.Filename ? prepared.NameSensitive : prepared.PathSensitive) : (scope == FilenameScope.Filename ? prepared.NameFolded : prepared.PathFolded)).AsSpan(); foreach (string token in tokens) { if (token.IndexOfAny(['*', '?']) >= 0 ? !Glob(target, token.AsSpan()) : !target.Contains(token.AsSpan(), StringComparison.Ordinal)) return false; } return true; }
    private static bool Glob(ReadOnlySpan<char> text, ReadOnlySpan<char> pattern) { int ti = 0, pi = 0, star = -1, starText = -1; while (ti < text.Length) { if (pi < pattern.Length) { OperationStatus ts = Rune.DecodeFromUtf16(text[ti..], out Rune tr, out int tw); OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw); if (ts == OperationStatus.Done && ps == OperationStatus.Done && pr.Value != '*' && pr.Value != '?' && tr == pr) { ti += tw; pi += pw; continue; } if (ps == OperationStatus.Done && pr.Value == '?') { ti += tw; pi += pw; continue; } if (ps == OperationStatus.Done && pr.Value == '*') { star = pi; starText = ti; pi += pw; continue; } } if (star < 0) return false; OperationStatus ss = Rune.DecodeFromUtf16(text[starText..], out _, out int sw); if (ss != OperationStatus.Done) return false; starText += sw; ti = starText; pi = star + 1; } while (pi < pattern.Length) { OperationStatus ps = Rune.DecodeFromUtf16(pattern[pi..], out Rune pr, out int pw); if (ps != OperationStatus.Done || pr.Value != '*') return false; pi += pw; } return true; }

    private static void ValidateIds(IReadOnlyList<FileRecord> source) { if (source.Select(r => r.FileId).Distinct().Count() != source.Count) throw new ArgumentException("FileId values must be unique"); }
    private static void WriteRecord(BinaryWriter writer, FileRecord r) { writer.Write(r.FileId); writer.Write(r.ParentId ?? 0); writer.Write(r.ParentId.HasValue); writer.Write(r.SizeBytes); writer.Write(r.ModifiedUtcTicks); writer.Write(r.Flags); writer.Write(r.Name); writer.Write(r.FullPath); }
    private static FileRecord ReadRecord(BinaryReader reader) { int id = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean(); ulong size = reader.ReadUInt64(); long modified = reader.ReadInt64(); byte flags = reader.ReadByte(); return new FileRecord(id, hasParent ? parent : null, reader.ReadString(), reader.ReadString(), size, modified, flags); }
    private static void WriteStrings(BinaryWriter writer, IReadOnlyList<string> values) { writer.Write(values.Count); foreach (string value in values) writer.Write(value); }
    private static string[] ReadStrings(BinaryReader reader, int expected) { int count = reader.ReadInt32(); if (count != expected) throw new InvalidDataException("Route C string count mismatch"); var values = new string[count]; for (int i = 0; i < count; i++) values[i] = reader.ReadString(); return values; }
    private static void WriteIndex(BinaryWriter writer, IReadOnlyDictionary<string, Posting> index) { writer.Write(index.Count); foreach ((string key, Posting posting) in index.OrderBy(p => p.Key, StringComparer.Ordinal)) { writer.Write(key); posting.Write(writer); } }
    private static Dictionary<string, Posting> ReadIndex(BinaryReader reader, int universe) { int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route C index count invalid"); var result = new Dictionary<string, Posting>(count, StringComparer.Ordinal); for (int i = 0; i < count; i++) result.Add(reader.ReadString(), Posting.Read(reader, universe)); return result; }
    private static void WriteVarUInt(BinaryWriter writer, uint value) { while (value >= 0x80) { writer.Write((byte)((value & 0x7F) | 0x80)); value >>= 7; } writer.Write((byte)value); }
    private static uint ReadVarUInt(BinaryReader reader) { uint value = 0; int shift = 0; while (true) { byte b = reader.ReadByte(); value |= (uint)(b & 0x7F) << shift; if ((b & 0x80) == 0) return value; shift += 7; if (shift > 28) throw new InvalidDataException("Route C varint is too long"); } }

    private sealed class Posting
    {
        private readonly int count; private readonly int[]? values; private readonly uint[]? bitmap;
        private Posting(int count, int[]? values, uint[]? bitmap) { this.count = count; this.values = values; this.bitmap = bitmap; }
        public static Posting List(int[] values) => new(values.Length, values, null);
        public static Posting Bitmap(IReadOnlyList<int> values, int universe) { var words = new uint[(universe + 31) / 32]; foreach (int value in values) words[value / 32] |= 1u << (value % 32); return new Posting(values.Count, null, words); }
        public int[] ToArray() { if (values is not null) return values; var result = new int[count]; int cursor = 0; for (int wordIndex = 0; wordIndex < bitmap!.Length; wordIndex++) { uint word = bitmap[wordIndex]; while (word != 0) { int bit = BitOperations.TrailingZeroCount(word); result[cursor++] = wordIndex * 32 + bit; word &= word - 1; } } return result; }
        public void Write(BinaryWriter writer) { writer.Write(values is not null ? (byte)0 : (byte)1); writer.Write(count); if (values is not null) { int previous = 0; foreach (int value in values) { WriteVarUInt(writer, checked((uint)(value - previous))); previous = value; } } else { writer.Write(bitmap!.Length); foreach (uint word in bitmap) writer.Write(word); } }
        public static Posting Read(BinaryReader reader, int universe) { byte kind = reader.ReadByte(); int count = reader.ReadInt32(); if (count < 0 || count > universe) throw new InvalidDataException("Route C posting count invalid"); if (kind == 0) { var values = new int[count]; int previous = 0; for (int i = 0; i < count; i++) { previous = checked(previous + (int)ReadVarUInt(reader)); values[i] = previous; } return new Posting(count, values, null); } if (kind == 1) { int wordCount = reader.ReadInt32(); if (wordCount != (universe + 31) / 32) throw new InvalidDataException("Route C bitmap length mismatch"); var words = new uint[wordCount]; for (int i = 0; i < wordCount; i++) words[i] = reader.ReadUInt32(); return new Posting(count, null, words); } throw new InvalidDataException("Route C posting kind invalid"); }
    }

    private readonly record struct Prepared(FileRecord Record, string NameSensitive, string PathSensitive, string NameFolded, string PathFolded);
}

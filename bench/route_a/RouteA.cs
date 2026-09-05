using System.Buffers;
using System.Diagnostics;
using System.Text;
using FilenameSearch.Core;

namespace FilenameSearch.RouteA;

/// <summary>Route A: compact normalized text blobs with a full scan and no substring posting index.</summary>
public sealed class RouteAEngine : IFilenameSearchEngine
{
    private const string Magic = "FRA001";
    private readonly object gate = new();
    private FileRecord[] records = [];
    private Prepared[] preparedRecords = [];
    private Blob nameSensitive = Blob.Empty, pathSensitive = Blob.Empty, nameFolded = Blob.Empty, pathFolded = Blob.Empty;
    private Dictionary<int, int> positions = [];
    private Dictionary<int, Prepared?> changes = [];

    public string RouteName => "A";
    public IReadOnlyList<FileRecord> Records { get { lock (gate) return SnapshotRecords(); } }

    public void Build(IReadOnlyList<FileRecord> source)
    {
        FileRecord[] next = source.OrderBy(r => r.FileId).ToArray();
        ValidateIds(next);
        Prepared[] prepared = next.Select(Prepare).ToArray();
        Blob[] blobs = BuildBlobs(prepared);
        lock (gate)
        {
            records = next; preparedRecords = prepared; nameSensitive = blobs[0]; pathSensitive = blobs[1]; nameFolded = blobs[2]; pathFolded = blobs[3];
            positions = next.Select((r, i) => (r.FileId, i)).ToDictionary(x => x.FileId, x => x.i);
            changes = [];
        }
    }

    public void Save(string store)
    {
        string fullPath = Path.GetFullPath(store); Directory.CreateDirectory(Path.GetDirectoryName(fullPath)!);
        FileRecord[] current;
        Blob[] blobs;
        lock (gate) { current = SnapshotRecords(); blobs = BuildBlobs(current.Select(Prepare).ToArray()); }
        using var stream = new FileStream(fullPath, FileMode.Create, FileAccess.Write, FileShare.Read);
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: false);
        writer.Write(Magic); writer.Write(1); writer.Write(current.Length);
        foreach (FileRecord record in current) WriteRecord(writer, record);
        foreach (Blob blob in blobs) WriteBlob(writer, blob);
    }

    public void Load(string store)
    {
        using var stream = new FileStream(store, FileMode.Open, FileAccess.Read, FileShare.Read);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: false);
        if (reader.ReadString() != Magic || reader.ReadInt32() != 1) throw new InvalidDataException("Route A store header mismatch");
        int count = reader.ReadInt32(); if (count < 0 || count > 10_000_000) throw new InvalidDataException("Route A store count invalid");
        var next = new FileRecord[count]; for (int i = 0; i < count; i++) next[i] = ReadRecord(reader);
        Blob[] blobs = [ReadBlob(reader, count), ReadBlob(reader, count), ReadBlob(reader, count), ReadBlob(reader, count)];
        if (stream.Position != stream.Length) throw new InvalidDataException("Route A store has trailing bytes");
        ValidateIds(next);
        var prepared = new Prepared[count];
        for (int i = 0; i < count; i++) prepared[i] = new Prepared(next[i], blobs[0].Get(i), blobs[1].Get(i), blobs[2].Get(i), blobs[3].Get(i));
        lock (gate)
        {
            records = next; preparedRecords = prepared; nameSensitive = blobs[0]; pathSensitive = blobs[1]; nameFolded = blobs[2]; pathFolded = blobs[3];
            positions = next.Select((r, i) => (r.FileId, i)).ToDictionary(x => x.FileId, x => x.i); changes = [];
        }
    }

    public SearchResult Search(FilenameQuery request)
    {
        if (request is null) throw new ArgumentNullException(nameof(request));
        var stopwatch = Stopwatch.StartNew();
        string[] tokens = request.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries)
            .Select(t => FilenameSemantics.Normalize(t, request.CaseSensitive)).ToArray();
        var matches = new List<FileRecord>(); int candidates = 0; bool requiresSort;
        lock (gate)
        {
            requiresSort = changes.Count > 0;
            for (int i = 0; i < records.Length; i++)
            {
                if (!TryCurrent(i, out Prepared prepared)) continue;
                candidates++;
                if (Matches(prepared, request.Scope, request.CaseSensitive, tokens)) matches.Add(prepared.Record);
            }
            foreach ((int fileId, Prepared? change) in changes)
            {
                if (positions.ContainsKey(fileId) || change is null) continue;
                candidates++;
                if (Matches(change, request.Scope, request.CaseSensitive, tokens)) matches.Add(change.Record);
            }
        }
        if (requiresSort) matches.Sort((left, right) => left.FileId.CompareTo(right.FileId));
        if (request.Limit > 0 && matches.Count > request.Limit) matches.RemoveRange(request.Limit, matches.Count - request.Limit);
        stopwatch.Stop();
        return new SearchResult(matches, stopwatch.Elapsed.TotalMilliseconds, candidates, true);
    }

    public void Upsert(FileRecord record)
    {
        Prepared prepared = Prepare(record);
        lock (gate) changes[record.FileId] = prepared;
    }

    public bool Remove(int fileId)
    {
        lock (gate)
        {
            bool existed = positions.ContainsKey(fileId) || (changes.TryGetValue(fileId, out Prepared? current) && current is not null);
            if (!existed) return false;
            changes[fileId] = null; return true;
        }
    }

    private bool TryCurrent(int index, out Prepared prepared)
    {
        int id = records[index].FileId;
        if (changes.TryGetValue(id, out Prepared? change)) { prepared = change!; return change is not null; }
        prepared = preparedRecords[index];
        return true;
    }

    private static bool Matches(Prepared prepared, FilenameScope scope, bool caseSensitive, string[] tokens)
    {
        ReadOnlySpan<char> target = (caseSensitive ? (scope == FilenameScope.Filename ? prepared.NameSensitive : prepared.PathSensitive) : (scope == FilenameScope.Filename ? prepared.NameFolded : prepared.PathFolded)).AsSpan();
        foreach (string token in tokens)
        {
            if (token.IndexOfAny(['*', '?']) >= 0)
            {
                if (!Glob(target, token.AsSpan())) return false;
            }
            else if (!target.Contains(token.AsSpan(), StringComparison.Ordinal)) return false;
        }
        return true;
    }

    private static bool Glob(ReadOnlySpan<char> text, ReadOnlySpan<char> pattern)
    {
        int textPosition = 0, patternPosition = 0, starPattern = -1, starText = -1;
        while (textPosition < text.Length)
        {
            if (patternPosition < pattern.Length)
            {
                OperationStatus textStatus = Rune.DecodeFromUtf16(text[textPosition..], out Rune textRune, out int textWidth);
                OperationStatus patternStatus = Rune.DecodeFromUtf16(pattern[patternPosition..], out Rune patternRune, out int patternWidth);
                if (textStatus == OperationStatus.Done && patternStatus == OperationStatus.Done && patternRune.Value != '*' && patternRune.Value != '?' && textRune == patternRune)
                { textPosition += textWidth; patternPosition += patternWidth; continue; }
                if (patternStatus == OperationStatus.Done && patternRune.Value == '?')
                { textPosition += textWidth; patternPosition += patternWidth; continue; }
                if (patternStatus == OperationStatus.Done && patternRune.Value == '*')
                { starPattern = patternPosition; starText = textPosition; patternPosition += patternWidth; continue; }
            }
            if (starPattern < 0) return false;
            OperationStatus starStatus = Rune.DecodeFromUtf16(text[starText..], out _, out int starWidth);
            if (starStatus != OperationStatus.Done) return false;
            starText += starWidth; textPosition = starText; patternPosition = starPattern + 1;
        }
        while (patternPosition < pattern.Length)
        {
            OperationStatus status = Rune.DecodeFromUtf16(pattern[patternPosition..], out Rune trailingRune, out int width);
            if (status != OperationStatus.Done) return false;
            if (trailingRune.Value != '*') return false;
            patternPosition += width;
        }
        return patternPosition == pattern.Length;
    }

    private FileRecord[] SnapshotRecords()
    {
        var snapshot = new List<FileRecord>(records.Length + changes.Count);
        for (int i = 0; i < records.Length; i++) if (TryCurrent(i, out Prepared prepared)) snapshot.Add(prepared.Record);
        foreach ((int fileId, Prepared? change) in changes) if (!positions.ContainsKey(fileId) && change is not null) snapshot.Add(change.Record);
        snapshot.Sort((left, right) => left.FileId.CompareTo(right.FileId)); return snapshot.ToArray();
    }

    private static Prepared Prepare(FileRecord record) => new(record, record.Name.Normalize(NormalizationForm.FormC), record.FullPath.Normalize(NormalizationForm.FormC), FilenameSemantics.Normalize(record.Name, false), FilenameSemantics.Normalize(record.FullPath, false));

    private static Blob[] BuildBlobs(IReadOnlyList<FileRecord> source)
    {
        Prepared[] prepared = source.Select(Prepare).ToArray();
        return BuildBlobs(prepared);
    }

    private static Blob[] BuildBlobs(IReadOnlyList<Prepared> prepared)
        => [Blob.Create(prepared.Select(p => p.NameSensitive)), Blob.Create(prepared.Select(p => p.PathSensitive)), Blob.Create(prepared.Select(p => p.NameFolded)), Blob.Create(prepared.Select(p => p.PathFolded))];

    private static void ValidateIds(IReadOnlyList<FileRecord> source)
    { if (source.Select(r => r.FileId).Distinct().Count() != source.Count) throw new ArgumentException("FileId values must be unique"); }

    private static void WriteRecord(BinaryWriter writer, FileRecord record)
    { writer.Write(record.FileId); writer.Write(record.ParentId ?? 0); writer.Write(record.ParentId.HasValue); writer.Write(record.SizeBytes); writer.Write(record.ModifiedUtcTicks); writer.Write(record.Flags); writer.Write(record.Name); writer.Write(record.FullPath); }
    private static FileRecord ReadRecord(BinaryReader reader)
    {
        int id = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean();
        ulong size = reader.ReadUInt64(); long modified = reader.ReadInt64(); byte flags = reader.ReadByte();
        return new FileRecord(id, hasParent ? parent : null, reader.ReadString(), reader.ReadString(), size, modified, flags);
    }
    private static void WriteBlob(BinaryWriter writer, Blob blob)
    { writer.Write(blob.Data.Length); writer.Write(new string(blob.Data)); writer.Write(blob.Offsets.Length); foreach (int value in blob.Offsets) writer.Write(value); foreach (int value in blob.Lengths) writer.Write(value); }
    private static Blob ReadBlob(BinaryReader reader, int count)
    { int charCount = reader.ReadInt32(); string data = reader.ReadString(); if (data.Length != charCount) throw new InvalidDataException("Route A blob length mismatch"); int n = reader.ReadInt32(); if (n != count) throw new InvalidDataException("Route A blob record count mismatch"); var offsets = new int[n]; var lengths = new int[n]; for (int i = 0; i < n; i++) offsets[i] = reader.ReadInt32(); for (int i = 0; i < n; i++) lengths[i] = reader.ReadInt32(); return new Blob(data.ToCharArray(), offsets, lengths); }

    private sealed record Prepared(FileRecord Record, string NameSensitive, string PathSensitive, string NameFolded, string PathFolded);

    private sealed class Blob(char[] data, int[] offsets, int[] lengths)
    {
        public static Blob Empty { get; } = new([], [], []);
        public char[] Data { get; } = data;
        public int[] Offsets { get; } = offsets;
        public int[] Lengths { get; } = lengths;
        public string Get(int index) => new(Data, Offsets[index], Lengths[index]);
        public static Blob Create(IEnumerable<string> values)
        {
            string[] strings = values.ToArray(); var offsets = new int[strings.Length]; var lengths = new int[strings.Length]; long total = 0;
            for (int i = 0; i < strings.Length; i++) { offsets[i] = checked((int)total); lengths[i] = strings[i].Length; total += strings[i].Length; }
            if (total > int.MaxValue) throw new InvalidDataException("Route A contiguous blob is too large");
            var data = new char[(int)total]; int cursor = 0;
            for (int i = 0; i < strings.Length; i++) { strings[i].AsSpan().CopyTo(data.AsSpan(cursor)); cursor += strings[i].Length; }
            return new Blob(data, offsets, lengths);
        }
    }
}

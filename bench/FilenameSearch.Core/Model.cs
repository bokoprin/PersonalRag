using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FilenameSearch.Core;

public enum FilenameScope { Filename, FullPath }

public sealed record FileRecord(
    int FileId,
    int? ParentId,
    string Name,
    string FullPath,
    ulong SizeBytes,
    long ModifiedUtcTicks,
    byte Flags = 1);

public sealed record FilenameQuery(
    string Query,
    FilenameScope Scope = FilenameScope.Filename,
    bool CaseSensitive = false,
    int Limit = 100);

public sealed record SearchResult(
    IReadOnlyList<FileRecord> Records,
    double ElapsedMs,
    int Candidates,
    bool UsedScan);

public interface IFilenameSearchEngine
{
    string RouteName { get; }
    IReadOnlyList<FileRecord> Records { get; }
    void Build(IReadOnlyList<FileRecord> records);
    void Load(string store);
    void Save(string store);
    SearchResult Search(FilenameQuery request);
    void Upsert(FileRecord record);
    bool Remove(int fileId);
}

public sealed record CorpusHeader(int Version, int RecordCount, long LogicalBytes, ulong Seed);

public sealed class CorpusData
{
    public required CorpusHeader Header { get; init; }
    public required FileRecord[] Records { get; init; }
}

public static class CorpusIO
{
    private static readonly byte[] Magic = "FRCAN001"u8.ToArray();

    public static void Write(string path, IReadOnlyList<FileRecord> records, long logicalBytes, ulong seed)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        using var stream = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.Read);
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: false);
        writer.Write(Magic);
        writer.Write(1);
        writer.Write(records.Count);
        writer.Write(logicalBytes);
        writer.Write(seed);
        foreach (var record in records) WriteRecord(writer, record);
    }

    public static CorpusData Read(string path)
    {
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: false);
        if (!reader.ReadBytes(Magic.Length).AsSpan().SequenceEqual(Magic)) throw new InvalidDataException("Filename corpus magic mismatch");
        int version = reader.ReadInt32(), count = reader.ReadInt32();
        long logicalBytes = reader.ReadInt64(); ulong seed = reader.ReadUInt64();
        if (version != 1 || count < 0 || count > 10_000_000 || logicalBytes < 0) throw new InvalidDataException("Filename corpus header is invalid");
        var records = new FileRecord[count];
        for (int i = 0; i < records.Length; i++) records[i] = ReadRecord(reader);
        if (stream.Position != stream.Length) throw new InvalidDataException("Filename corpus has trailing bytes");
        return new CorpusData { Header = new CorpusHeader(version, count, logicalBytes, seed), Records = records };
    }

    public static void WriteJson<T>(string path, T value)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        File.WriteAllText(path, JsonSerializer.Serialize(value, new JsonSerializerOptions
        {
            WriteIndented = true,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
        }));
    }

    private static void WriteRecord(BinaryWriter writer, FileRecord record)
    {
        writer.Write(record.FileId); writer.Write(record.ParentId ?? 0); writer.Write(record.ParentId.HasValue);
        writer.Write(record.SizeBytes); writer.Write(record.ModifiedUtcTicks); writer.Write(record.Flags);
        writer.Write(record.Name); writer.Write(record.FullPath);
    }

    private static FileRecord ReadRecord(BinaryReader reader)
    {
        int fileId = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean();
        ulong size = reader.ReadUInt64(); long modified = reader.ReadInt64(); byte flags = reader.ReadByte();
        return new FileRecord(fileId, hasParent ? parent : null, reader.ReadString(), reader.ReadString(), size, modified, flags);
    }
}

using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using FilenameSearch.Core;

namespace FilenameSearch.CorpusGenerator;

public sealed class SplitMix64(ulong seed)
{
    private ulong state = seed;
    public ulong Next()
    {
        state += 0x9E3779B97F4A7C15UL;
        ulong z = state;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
        return z ^ (z >> 31);
    }
    public int NextInt(int exclusiveMax) => (int)(Next() % (uint)exclusiveMax);
    public bool Chance(int numerator, int denominator) => NextInt(denominator) < numerator;
}

public sealed record CorpusManifest(
    int Version,
    int RecordCount,
    long LogicalBytes,
    string Seed,
    string GeneratorSha256,
    string RecordsSha256,
    string QuerySetSha256,
    DateTime CreatedUtc,
    string Kind);

public static class SyntheticCorpusGenerator
{
    public const ulong CalibrationSeed = 0x505241475F43414CUL;
    public const ulong OfficialSeed = 0x505241475F314D31UL;
    public const long TenGiB = 10L * 1024 * 1024 * 1024;
    public const long HundredGiB = 100L * 1024 * 1024 * 1024;
    private static readonly string[] Roots = ["C:\\Users", "C:\\Work", "C:\\Projects", "C:\\Source", "C:\\Documents", "C:\\Data", "D:\\Archive", "D:\\Projects", "D:\\Documents", "D:\\Build", "E:\\Shared", "E:\\Backup"];
    private static readonly string[] DirectoryTokens = ["src", "source", "include", "tests", "test", "docs", "documents", "config", "settings", "data", "dataset", "logs", "build", "release", "debug", "project", "projects", "module", "modules", "assets", "resources", "backup", "archive", "temp", "cache", "report", "reports", "invoice", "meeting", "analysis", "design", "spec", "仕様", "設計", "資料", "議事録", "検索", "開発", "テスト", "顧客", "装置", "ログ", "データ"];
    private static readonly string[] FilenameTokens = ["main", "app", "application", "readme", "config", "settings", "report", "invoice", "meeting", "notes", "design", "spec", "test", "tests", "result", "results", "backup", "archive", "data", "dataset", "index", "search", "personalrag", "runtime", "service", "client", "server", "module", "model", "view", "controller", "document", "manual", "sample", "example", "analysis", "metrics", "benchmark", "trace", "history", "2024", "2025", "2026", "設計", "仕様", "資料", "議事録", "検索", "開発", "試験", "結果", "顧客", "装置", "障害", "ログ", "データ"];
    private static readonly (int Max, string Extension)[] Extensions = [(12, ".txt"), (20, ".log"), (28, ".md"), (36, ".cs"), (44, ".cpp"), (50, ".h"), (56, ".rs"), (62, ".py"), (68, ".json"), (73, ".xml"), (78, ".yaml"), (83, ".toml"), (87, ".csv"), (91, ".xlsx"), (95, ".docx"), (97, ".pptx"), (99, ".pdf"), (100, "")];

    public static CorpusManifest Generate(string outputDirectory, int count, long logicalBytes, ulong seed, string kind, string generatorPath, string querySetPath = "")
    {
        outputDirectory = Path.GetFullPath(outputDirectory); Directory.CreateDirectory(outputDirectory);
        string recordsPath = Path.Combine(outputDirectory, "records.bin");
        var random = new SplitMix64(seed); long rawTotal = 0; int lastId = 0;
        using (var stream = new FileStream(recordsPath, FileMode.Create, FileAccess.ReadWrite, FileShare.Read))
        using (var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true))
        {
            writer.Write("FRCAN001"u8.ToArray()); writer.Write(1); writer.Write(count); writer.Write(logicalBytes); writer.Write(seed);
            string? duplicate = null;
            for (int id = 1; id <= count; id++)
            {
                var record = MakeRecord(id, count, random, duplicate);
                duplicate = id % 1000 == 0 && id < count ? record.Name : null;
                writer.Write(record.FileId); writer.Write(record.ParentId ?? 0); writer.Write(record.ParentId.HasValue);
                writer.Write(record.SizeBytes);
                writer.Write(record.ModifiedUtcTicks); writer.Write(record.Flags); writer.Write(record.Name); writer.Write(record.FullPath);
                rawTotal = checked(rawTotal + (long)record.SizeBytes); lastId = id;
            }
        }
        if (lastId != count) throw new InvalidDataException("Corpus count mismatch");
        RewriteScaledSizes(recordsPath, count, logicalBytes, rawTotal);
        string recordsHash = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(recordsPath))).ToLowerInvariant();
        string generatorHash = File.Exists(generatorPath) ? Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(generatorPath))).ToLowerInvariant() : "generated-in-process";
        string queryHash = querySetPath.Length > 0 && File.Exists(querySetPath) ? Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(querySetPath))).ToLowerInvariant() : "pending";
        var manifest = new CorpusManifest(1, count, logicalBytes, $"0x{seed:X16}", generatorHash, recordsHash, queryHash, DateTime.UtcNow, kind);
        CorpusIO.WriteJson(Path.Combine(outputDirectory, "corpus_manifest.json"), manifest);
        return manifest;
    }

    private static void RewriteScaledSizes(string path, int count, long logicalBytes, long rawTotal)
    {
        if (logicalBytes < count) throw new ArgumentOutOfRangeException(nameof(logicalBytes), "Logical bytes must allow at least one byte per record");
        using var stream = new FileStream(path, FileMode.Open, FileAccess.ReadWrite, FileShare.Read);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: true);
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true);
        _ = reader.ReadBytes(8); _ = reader.ReadInt32(); _ = reader.ReadInt32(); _ = reader.ReadInt64(); _ = reader.ReadUInt64();
        long scaledTotal = 0;
        for (int index = 0; index < count; index++)
        {
            _ = reader.ReadInt32(); _ = reader.ReadInt32(); _ = reader.ReadBoolean();
            long sizePosition = stream.Position;
            ulong rawSize = reader.ReadUInt64();
            _ = reader.ReadInt64(); _ = reader.ReadByte(); _ = reader.ReadString(); _ = reader.ReadString();
            ulong scaled = index == count - 1
                ? checked((ulong)(logicalBytes - scaledTotal))
                : Math.Max(1UL, (ulong)decimal.Truncate((decimal)rawSize * logicalBytes / rawTotal));
            if (scaled == 0) throw new InvalidDataException("Scaled record size became zero");
            scaledTotal = checked(scaledTotal + (long)scaled);
            long endPosition = stream.Position;
            stream.Position = sizePosition; writer.Write(scaled); writer.Flush(); stream.Position = endPosition;
        }
        if (scaledTotal != logicalBytes) throw new InvalidDataException($"Scaled logical size mismatch: {scaledTotal} != {logicalBytes}");
    }

    private static FileRecord MakeRecord(int id, int count, SplitMix64 random, string? duplicate)
    {
        string root = Roots[random.NextInt(Roots.Length)]; int depth = 2 + random.NextInt(7); var directories = new List<string>(depth);
        for (int i = 0; i < depth; i++) directories.Add(DirectorySegment(random));
        string extension = ChooseExtension(random.NextInt(100)); string name = duplicate ?? Filename(random, id, extension);
        if (id % 17 == 0) { name = ToggleAscii(name); directories = directories.Select(ToggleAscii).ToList(); }
        if (id % 40_000 == 0) name = Path.GetFileNameWithoutExtension(name) + (id / 40_000 % 2 == 0 ? "_café" : "_cafe\u0301") + extension;
        string fullPath = root + "\\" + string.Join('\\', directories) + "\\" + name;
        ulong size = SizeFor(random);
        // Keep the canonical corpus byte-for-byte reproducible across runs and dates.
        long modified = new DateTime(2020, 1, 1, 0, 0, 0, DateTimeKind.Utc).AddMinutes(id).Ticks;
        return new FileRecord(id, null, name, fullPath, size, modified, 1);
    }

    private static string DirectorySegment(SplitMix64 random)
    {
        string token = DirectoryTokens[random.NextInt(DirectoryTokens.Length)];
        if (random.Chance(1, 10)) return token;
        if (random.Chance(1, 18)) return token + "-" + DirectoryTokens[random.NextInt(DirectoryTokens.Length)];
        return token + "_" + ToBase36(random.NextInt(46656));
    }

    private static string Filename(SplitMix64 random, int id, string extension)
    {
        int count = 2 + random.NextInt(3); string separator = random.NextInt(2) == 0 ? "_" : "-"; var tokens = new List<string>(count);
        for (int i = 0; i < count; i++) tokens.Add(FilenameTokens[random.NextInt(FilenameTokens.Length)]);
        if (id % 2 == 0) tokens.Add("th"); if (id % 5 == 0) tokens.Add("in"); if (id % 100 == 0) tokens.Add("report");
        if (id % 1000 == 0) tokens.Add("personalrag"); if (id % 10_000 == 0) tokens.Add("zxqv7319");
        if (id % 50_000 == 0) tokens.Add("needle_kappa_9901"); if (id % 25_000 == 0) tokens.Add("唯一針"); if (id % 31 == 0) tokens.Add("README");
        return string.Join(separator, tokens) + "_" + ToBase36(id % 46656) + extension;
    }

    private static ulong SizeFor(SplitMix64 random)
    {
        int bucket = random.NextInt(100); return bucket switch
        {
            < 40 => (ulong)(1024 + random.NextInt(64 * 1024 - 1024 + 1)),
            < 70 => (ulong)(64 * 1024 + random.NextInt(1024 * 1024 - 64 * 1024 + 1)),
            < 90 => (ulong)(1024 * 1024 + random.NextInt(8 * 1024 * 1024 - 1024 * 1024 + 1)),
            < 98 => (ulong)(8 * 1024 * 1024 + random.NextInt(64 * 1024 * 1024 - 8 * 1024 * 1024 + 1)),
            _ => (ulong)(64 * 1024 * 1024 + random.NextInt(512 * 1024 * 1024 - 64 * 1024 * 1024 + 1))
        };
    }

    private static string ChooseExtension(int value)
    { foreach (var (max, extension) in Extensions) if (value < max) return extension; return ""; }
    private static string ToggleAscii(string value)
    { var chars = value.ToCharArray(); bool upper = true; for (int i = 0; i < chars.Length; i++) if (char.IsAsciiLetter(chars[i])) { chars[i] = upper ? char.ToUpperInvariant(chars[i]) : char.ToLowerInvariant(chars[i]); upper = !upper; } return new string(chars); }
    private static string ToBase36(int value)
    {
        const string alphabet = "0123456789abcdefghijklmnopqrstuvwxyz";
        if (value < 0) throw new ArgumentOutOfRangeException(nameof(value));
        Span<char> buffer = stackalloc char[8]; int cursor = buffer.Length;
        do { buffer[--cursor] = alphabet[value % 36]; value /= 36; } while (value > 0);
        return new string(buffer[cursor..]).PadLeft(3, '0');
    }
}

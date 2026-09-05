using System.Text.Json;
using System.Text.Json.Serialization;
using FilenameSearch.Core;

namespace FilenameSearch.QueryGenerator;

public sealed record QuerySpec(
    string Id,
    string Class,
    string Query,
    FilenameScope Scope = FilenameScope.Filename,
    bool CaseSensitive = false,
    int Limit = 1_000_000);

public sealed record QuerySetDocument(
    int Version,
    string CorpusSha256,
    IReadOnlyList<QuerySpec> Queries);

public static class QuerySetBuilder
{
    private static readonly int[] SampleIds = [123, 10007, 77777, 131071, 222223, 333331, 444443, 555557, 666667, 777779, 888889, 999983];

    public static QuerySetDocument Build(CorpusData corpus, string corpusSha256)
    {
        var queries = new List<QuerySpec>();
        Add(queries, "literal-1", ["a", "e", "r", "t", "1", "2", "_", "-", "設", "検", "デ"]);
        Add(queries, "literal-2", ["th", "in", "re", "20", "ra", "on", "設計", "検索", "デー"]);
        Add(queries, "common", ["report", "config", "project", "data", "test", "source", "backup", "2026", "design", "search", "設計", "資料", "ログ"]);
        Add(queries, "medium-long", ["personalrag", "application", "benchmark", "controller", "documents", "needle", "議事録", "障害"]);
        Add(queries, "rare", ["zxqv7319", "needle_kappa_9901", "唯一針"]);
        Add(queries, "zero-hit", ["__NO_HIT_7F31C2__", "zzzz_unique_absent_991827", "存在しない検索語_8841"]);
        Add(queries, "case-insensitive", ["README", "readme", "PersonalRag", "personalrag", "PERSONALRAG"], caseSensitive: false);
        Add(queries, "case-sensitive", ["README", "readme", "PersonalRag", "personalrag"], caseSensitive: true);
        Add(queries, "wildcard", ["*.rs", "*.cpp", "*.xlsx", "report_*.xlsx", "*personalrag*", "*2026*", "???.log", "設計*.docx", "*検索*"]);
        Add(queries, "path-literal", ["\\src_", "\\documents_", "\\project_", "\\backup_", "\\設計_", "\\検索_", "personalrag", "2026", "D:\\Projects", "C:\\Source"], FilenameScope.FullPath);
        Add(queries, "path-wildcard", ["*\\src_*\\*.rs", "*\\reports_*\\*", "D:\\Projects\\*", "C:\\Documents\\*", "*\\設計_*\\*.docx"], FilenameScope.FullPath);
        Add(queries, "and", ["personalrag 2026", "report xlsx", "design test", "設計 2026", "search benchmark"]);

        foreach (int fileId in SampleIds)
        {
            if (fileId > corpus.Records.Length) continue;
            FileRecord record = corpus.Records[fileId - 1];
            string stem = Path.GetFileNameWithoutExtension(record.Name);
            AddCenter(queries, $"sample-{fileId}-stem-3", "sampled-stem-3", stem, 3);
            AddCenter(queries, $"sample-{fileId}-stem-5", "sampled-stem-5", stem, 5);
            AddCenter(queries, $"sample-{fileId}-stem-9", "sampled-stem-9", stem, 9);
            AddCenter(queries, $"sample-{fileId}-path-7", "sampled-path-7", record.FullPath, 7, FilenameScope.FullPath);
            string segment = record.FullPath.Split('\\', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault(s => !s.Contains(':')) ?? record.Name;
            queries.Add(new QuerySpec($"sample-{fileId}-directory", "sampled-directory", segment, FilenameScope.FullPath));
        }
        return new QuerySetDocument(1, corpusSha256, queries);
    }

    public static void Write(string path, QuerySetDocument document)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        var options = new JsonSerializerOptions
        {
            WriteIndented = true,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
        };
        File.WriteAllText(path, JsonSerializer.Serialize(document, options));
    }

    private static void Add(List<QuerySpec> queries, string category, IEnumerable<string> values, FilenameScope scope = FilenameScope.Filename, bool caseSensitive = false)
    {
        int index = 0;
        foreach (string value in values)
        {
            queries.Add(new QuerySpec($"{category}-{index++:D3}", category, value, scope, caseSensitive));
        }
    }

    private static void AddCenter(List<QuerySpec> queries, string id, string category, string value, int codePoints, FilenameScope scope = FilenameScope.Filename)
    {
        var runes = value.EnumerateRunes().ToArray();
        if (runes.Length < codePoints) throw new InvalidDataException($"Sample value for {id} is shorter than {codePoints} code points");
        int start = Math.Max(0, (runes.Length - codePoints) / 2);
        queries.Add(new QuerySpec(id, category, string.Concat(runes.Skip(start).Take(codePoints).Select(r => r.ToString())), scope));
    }
}

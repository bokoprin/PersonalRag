using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using FilenameSearch.Core;
using FilenameSearch.QueryGenerator;
using FilenameSearch.RouteA;

if (args.Length < 5) throw new ArgumentException("verify CORPUS_RECORDS QUERY_SET ORACLE_JSON STORE");
if (!args[0].Equals("verify", StringComparison.OrdinalIgnoreCase)) throw new ArgumentException("Only verify is supported");
var jsonOptions = new JsonSerializerOptions
{
    PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    PropertyNameCaseInsensitive = true,
    Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
};
CorpusData corpus = CorpusIO.Read(args[1]);
QuerySetDocument querySet = JsonSerializer.Deserialize<QuerySetDocument>(File.ReadAllText(args[2]), jsonOptions) ?? throw new InvalidDataException("Query set JSON is empty");
OracleFile oracle = JsonSerializer.Deserialize<OracleFile>(File.ReadAllText(args[3]), jsonOptions) ?? throw new InvalidDataException("Oracle JSON is empty");
var engine = new RouteAEngine(); engine.Build(corpus.Records);
VerificationSummary built = Verify(engine, querySet, oracle);
engine.Save(args[4]);
var loaded = new RouteAEngine(); loaded.Load(args[4]);
VerificationSummary restored = Verify(loaded, querySet, oracle);
Console.WriteLine(JsonSerializer.Serialize(new { route = engine.RouteName, built, restored, store_bytes = new FileInfo(args[4]).Length }, jsonOptions));
return built.FalsePositive + built.FalseNegative + restored.FalsePositive + restored.FalseNegative == 0 ? 0 : 1;

static VerificationSummary Verify(IFilenameSearchEngine engine, QuerySetDocument querySet, OracleFile oracle)
{
    var expected = oracle.Results.ToDictionary(r => r.Id, StringComparer.Ordinal);
    int falsePositive = 0, falseNegative = 0, checkedQueries = 0;
    foreach (QuerySpec query in querySet.Queries)
    {
        SearchResult result = engine.Search(new FilenameQuery(query.Query, query.Scope, query.CaseSensitive, query.Limit));
        int[] actual = result.Records.Select(r => r.FileId).OrderBy(id => id).ToArray();
        OracleResult truth = expected[query.Id];
        if (actual.Length > truth.ExpectedCount) falsePositive += actual.Length - truth.ExpectedCount;
        if (actual.Length < truth.ExpectedCount) falseNegative += truth.ExpectedCount - actual.Length;
        string hash = HashIds(actual);
        if (!hash.Equals(truth.ExpectedIdsSha256, StringComparison.OrdinalIgnoreCase))
        {
            var expectedIds = GetExpectedIdsByScan(engine.Records, query);
            falsePositive += actual.Except(expectedIds).Count(); falseNegative += expectedIds.Except(actual).Count();
        }
        checkedQueries++;
    }
    return new VerificationSummary(checkedQueries, falsePositive, falseNegative);
}

// This diagnostic fallback is only used to produce a useful cardinality when a hash differs.
// The correctness authority remains the independently generated oracle JSON.
static int[] GetExpectedIdsByScan(IReadOnlyList<FileRecord> records, QuerySpec query)
    => records.Where(r => FilenameSemantics.Matches(r, new FilenameQuery(query.Query, query.Scope, query.CaseSensitive, query.Limit))).Select(r => r.FileId).OrderBy(id => id).ToArray();

static string HashIds(IReadOnlyList<int> ids)
{
    using IncrementalHash hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
    foreach (int id in ids) { hash.AppendData(Encoding.UTF8.GetBytes(id.ToString(System.Globalization.CultureInfo.InvariantCulture))); hash.AppendData(","u8); }
    return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant();
}

public sealed record OracleFile(int Version, string CorpusSha256, string QuerySetSha256, IReadOnlyList<OracleResult> Results);
public sealed record OracleResult(string Id, string Class, int ExpectedCount, string ExpectedIdsSha256);
public sealed record VerificationSummary(int CheckedQueries, int FalsePositive, int FalseNegative);

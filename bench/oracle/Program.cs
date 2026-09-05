using System.Security.Cryptography;
using System.Text.Json;
using FilenameSearch.Core;
using FilenameSearch.Oracle;
using FilenameSearch.QueryGenerator;

if (args.Length < 3) throw new ArgumentException("evaluate CORPUS_RECORDS QUERY_SET OUTPUT_JSON");
if (!args[0].Equals("evaluate", StringComparison.OrdinalIgnoreCase)) throw new ArgumentException("Only evaluate is supported");
var options = new JsonSerializerOptions
{
    PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    PropertyNameCaseInsensitive = true,
    Converters = { new System.Text.Json.Serialization.JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
};
CorpusData corpus = CorpusIO.Read(args[1]);
QuerySetDocument querySet = JsonSerializer.Deserialize<QuerySetDocument>(File.ReadAllText(args[2]), options) ?? throw new InvalidDataException("Query set JSON is empty");
string corpusSha = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(args[1]))).ToLowerInvariant();
string querySetSha = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(args[2]))).ToLowerInvariant();
OracleDocument document = IndependentOracle.Evaluate(corpus, querySet, corpusSha, querySetSha);
IndependentOracle.Write(args[3], document);
Console.WriteLine($"Evaluated {document.Results.Count} queries");

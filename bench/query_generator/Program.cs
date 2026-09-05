using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Nodes;
using FilenameSearch.Core;
using FilenameSearch.QueryGenerator;

if (args.Length < 3) throw new ArgumentException("generate CORPUS_RECORDS OUTPUT_JSON");
if (!args[0].Equals("generate", StringComparison.OrdinalIgnoreCase)) throw new ArgumentException("Only generate is supported");
CorpusData corpus = CorpusIO.Read(args[1]);
string corpusSha = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(args[1]))).ToLowerInvariant();
QuerySetDocument document = QuerySetBuilder.Build(corpus, corpusSha);
QuerySetBuilder.Write(args[2], document);
string querySha = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(args[2]))).ToLowerInvariant();
string manifestPath = Path.Combine(Path.GetDirectoryName(Path.GetFullPath(args[1]))!, "corpus_manifest.json");
if (File.Exists(manifestPath))
{
    JsonObject manifest = JsonNode.Parse(File.ReadAllText(manifestPath))?.AsObject() ?? throw new InvalidDataException("Corpus manifest JSON is empty");
    manifest["query_set_sha256"] = querySha;
    File.WriteAllText(manifestPath, manifest.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
}
Console.WriteLine($"Generated {document.Queries.Count} queries; corpus_sha256={corpusSha}");

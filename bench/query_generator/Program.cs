using System.Security.Cryptography;
using FilenameSearch.Core;
using FilenameSearch.QueryGenerator;

if (args.Length < 3) throw new ArgumentException("generate CORPUS_RECORDS OUTPUT_JSON");
if (!args[0].Equals("generate", StringComparison.OrdinalIgnoreCase)) throw new ArgumentException("Only generate is supported");
CorpusData corpus = CorpusIO.Read(args[1]);
string corpusSha = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(args[1]))).ToLowerInvariant();
QuerySetDocument document = QuerySetBuilder.Build(corpus, corpusSha);
QuerySetBuilder.Write(args[2], document);
Console.WriteLine($"Generated {document.Queries.Count} queries; corpus_sha256={corpusSha}");

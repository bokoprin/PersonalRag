using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using PersonalRag.ContentSearch.Backends.Bloom;
using PersonalRag.ContentSearch.Backends.Scan;
using PersonalRag.ContentSearch.Backends.Sqlite;
using PersonalRag.ContentSearch.Backends.Trigram;
using PersonalRag.ContentSearch.Bakeoff;
using PersonalRag.ContentSearch.Core;

Encoding.RegisterProvider(CodePagesEncodingProvider.Instance);

if (FormalRunner.IsFormal(args))
{
    await FormalRunner.RunAsync(args);
    return;
}

Arguments parsed = Arguments.Parse(args);
string root = parsed.Root ?? Path.Combine(Path.GetTempPath(), "PersonalRag-ContentBakeoff-Smoke");
string work = parsed.Work ?? Path.Combine(Path.GetTempPath(), "PersonalRag-ContentBakeoff-Work");
string reportPath = parsed.Report ?? Path.Combine(work, "CONTENT_SEARCH_BAKEOFF_SMOKE.json");
bool ownsCorpus = parsed.Root is null;

if (ownsCorpus)
{
    if (Directory.Exists(root)) Directory.Delete(root, recursive: true);
    Directory.CreateDirectory(root);
    await CreateSmokeCorpusAsync(root);
}

Directory.CreateDirectory(work);
IReadOnlyList<ContentDocument> documents = BuildDocuments(root);
var corpus = new ContentCorpus(documents, work, BlockSizeChars: 65_536, OverlapChars: 256);
IReadOnlyList<ContentQuery> queries = LoadQueries();
Dictionary<string, int> smokeOracle = LoadSmokeOracle();

var backends = CreateBackends(parsed.Backend);
var backendReports = new List<BackendReport>();
Dictionary<string, HashSet<string>>? oracle = null;
string? mutablePath = ownsCorpus ? Path.Combine(root, "mutable.log") : null;

try
{
    foreach (IContentSearchBackend backend in backends)
    {
        Console.WriteLine($"[{backend.BackendId}] build");
        long privateBefore = Process.GetCurrentProcess().PrivateMemorySize64;
        var buildWatch = Stopwatch.StartNew();
        await backend.BuildAsync(corpus, CancellationToken.None);
        buildWatch.Stop();
        long privateAfter = Process.GetCurrentProcess().PrivateMemorySize64;

        var queryReports = new List<QueryReport>();
        var currentSignatures = new Dictionary<string, HashSet<string>>(StringComparer.Ordinal);

        foreach (ContentQuery query in queries)
        {
            var samples = new List<double>(20);
            IReadOnlyList<ContentMatch> last = Array.Empty<ContentMatch>();

            for (int round = 0; round < 20; round++)
            {
                var sw = Stopwatch.StartNew();
                last = await backend.SearchAsync(query, CancellationToken.None);
                sw.Stop();
                samples.Add(sw.Elapsed.TotalMilliseconds);
            }

            HashSet<string> signature = Signature(last);
            string queryKey = QueryKey(query);
            currentSignatures[queryKey] = signature;
            if (ownsCorpus &&
                smokeOracle.TryGetValue(queryKey, out int expectedCount) &&
                last.Count != expectedCount)
            {
                throw new InvalidOperationException(
                    $"{backend.BackendId} failed independent smoke oracle for {queryKey}. " +
                    $"expected={expectedCount}, actual={last.Count}");
            }

            double[] measured = samples.Skip(2).OrderBy(v => v).ToArray();
            queryReports.Add(new QueryReport(
                queryKey,
                last.Count,
                Percentile(measured, 0.50),
                Percentile(measured, 0.95),
                Percentile(measured, 0.99),
                measured.Length == 0 ? 0 : measured[^1]));
        }

        if (backend is DirectScanBackend)
        {
            oracle = currentSignatures;
        }
        else if (oracle is not null)
        {
            foreach ((string key, HashSet<string> expected) in oracle)
            {
                if (!currentSignatures.TryGetValue(key, out HashSet<string>? actual) ||
                    !expected.SetEquals(actual))
                {
                    throw new InvalidOperationException(
                        $"{backend.BackendId} differs from DirectScan oracle for {key}. " +
                        $"expected={expected.Count}, actual={actual?.Count ?? -1}");
                }
            }
        }

        bool updatePass = true;
        if (ownsCorpus && mutablePath is not null)
        {
            const string updateToken = "BAKEOFF_UPDATE_TOKEN_91F6C7";
            await File.AppendAllTextAsync(mutablePath, Environment.NewLine + updateToken, Encoding.UTF8);
            var info = new FileInfo(mutablePath);
            ContentDocument updated = CreateDocument(mutablePath, info);

            await backend.ApplyChangesAsync(
                new[] { new ContentChange(ContentChangeKind.Updated, updated, updated.FileKey) },
                CancellationToken.None);

            IReadOnlyList<ContentMatch> updateMatches = await backend.SearchAsync(
                new ContentQuery(updateToken, ContentQueryMode.Substring, CaseSensitive: true),
                CancellationToken.None);
            updatePass = updateMatches.Any(m =>
                string.Equals(m.ExactPath, mutablePath, StringComparison.OrdinalIgnoreCase));

            await File.WriteAllTextAsync(
                mutablePath,
                "mutable file before update" + Environment.NewLine,
                Encoding.UTF8);
        }

        ContentBackendDiagnostics diagnostics = backend.GetDiagnostics();
        backendReports.Add(new BackendReport(
            backend.BackendId,
            buildWatch.Elapsed.TotalMilliseconds,
            Math.Max(0, privateAfter - privateBefore),
            diagnostics.PersistentBytes,
            diagnostics.BytesRead,
            diagnostics.CandidateBlocks,
            diagnostics.VerifiedBlocks,
            diagnostics.ScanFallbackCount,
            updatePass,
            queryReports));

        Console.WriteLine(
            $"[{backend.BackendId}] build={buildWatch.Elapsed.TotalMilliseconds:F1}ms " +
            $"persistent={diagnostics.PersistentBytes:N0} bytes update={updatePass}");
    }
}
finally
{
    foreach (IContentSearchBackend backend in backends)
        await backend.DisposeAsync();
}

var finalReport = new
{
    version = 1,
    generatedUtc = DateTime.UtcNow,
    root,
    work,
    ownsCorpus,
    documentCount = documents.Count,
    blockSizeChars = corpus.BlockSizeChars,
    overlapChars = corpus.OverlapChars,
    rounds = 20,
    warmupRounds = 2,
    measuredRounds = 18,
    backends = backendReports
};

Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(reportPath))!);
await File.WriteAllTextAsync(
    reportPath,
    JsonSerializer.Serialize(finalReport, new JsonSerializerOptions { WriteIndented = true }),
    Encoding.UTF8);

Console.WriteLine($"Report: {reportPath}");

static List<IContentSearchBackend> CreateBackends(string backend)
{
    var all = new List<IContentSearchBackend>
    {
        new DirectScanBackend(),
        new BloomFilterBackend(),
        new TrigramInvertedBackend(),
        new SqliteFts5Backend()
    };

    if (string.Equals(backend, "all", StringComparison.OrdinalIgnoreCase))
        return all;

    IContentSearchBackend? selected = all.FirstOrDefault(
        b => string.Equals(b.BackendId, backend, StringComparison.OrdinalIgnoreCase));
    if (selected is null)
        throw new ArgumentException($"Unknown backend: {backend}");

    foreach (IContentSearchBackend item in all)
    {
        if (!ReferenceEquals(item, selected))
            item.DisposeAsync().AsTask().GetAwaiter().GetResult();
    }

    return new List<IContentSearchBackend> { selected };
}

static IReadOnlyList<ContentDocument> BuildDocuments(string root)
{
    return Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)
        .Where(TextExtraction.IsSupportedPath)
        .OrderBy(path => path, StringComparer.Ordinal)
        .Select(path =>
        {
            var info = new FileInfo(path);
            return CreateDocument(path, info);
        })
        .ToArray();
}

static ContentDocument CreateDocument(string path, FileInfo info)
{
    string full = Path.GetFullPath(path);
    byte[] hash = SHA256.HashData(Encoding.UTF8.GetBytes(full.ToUpperInvariant()));
    return new ContentDocument(
        new ContentFileKey(Convert.ToHexString(hash)),
        full,
        info.Length,
        info.LastWriteTimeUtc);
}

static Dictionary<string, int> LoadSmokeOracle()
{
    string path = Path.Combine(AppContext.BaseDirectory, "oracle-v1.json");
    if (!File.Exists(path))
        return new Dictionary<string, int>(StringComparer.Ordinal);

    using JsonDocument json = JsonDocument.Parse(File.ReadAllText(path));
    var result = new Dictionary<string, int>(StringComparer.Ordinal);
    if (!json.RootElement.TryGetProperty("smokeExpectedCounts", out JsonElement counts))
        return result;

    foreach (JsonProperty property in counts.EnumerateObject())
        result[property.Name] = property.Value.GetInt32();
    return result;
}

static IReadOnlyList<ContentQuery> LoadQueries()
{
    string path = Path.Combine(AppContext.BaseDirectory, "query-set.json");
    if (!File.Exists(path))
    {
        return new[]
        {
            new ContentQuery("GenerationStore"),
            new ContentQuery("error"),
            new ContentQuery("障害"),
            new ContentQuery("SS"),
            new ContentQuery("if"),
            new ContentQuery("x", ContentQueryMode.Substring, true)
        };
    }

    using JsonDocument json = JsonDocument.Parse(File.ReadAllText(path));
    return json.RootElement.EnumerateArray()
        .Select(element =>
        {
            string text = element.GetProperty("text").GetString() ?? string.Empty;
            string modeText = element.GetProperty("mode").GetString() ?? "Substring";
            bool caseSensitive = element.TryGetProperty("caseSensitive", out JsonElement cs) && cs.GetBoolean();
            ContentQueryMode mode = Enum.Parse<ContentQueryMode>(modeText, ignoreCase: true);
            return new ContentQuery(text, mode, caseSensitive);
        })
        .ToArray();
}

static async Task CreateSmokeCorpusAsync(string root)
{
    await File.WriteAllTextAsync(
        Path.Combine(root, "code.cs"),
        """
        namespace Smoke;
        internal sealed class GenerationStore
        {
            public void Load() { var error = "0xA1B2C3D4"; }
        }
        """,
        new UTF8Encoding(false));

    await File.WriteAllTextAsync(
        Path.Combine(root, "japanese.log"),
        "装置で障害が発生しました。\nGenerationIndex recovery completed.\n",
        new UTF8Encoding(false));

    await File.WriteAllTextAsync(
        Path.Combine(root, "unicode.txt"),
        "Straße STRASSE ẞ ss ﬃ ffi Σ σ ς Kelvin K K\n",
        new UTF8Encoding(false));

    Encoding cp932 = Encoding.GetEncoding(932);
    await File.WriteAllTextAsync(
        Path.Combine(root, "legacy.txt"),
        "CP932 日本語ログ error code\n",
        cp932);

    await File.WriteAllTextAsync(
        Path.Combine(root, "mutable.log"),
        "mutable file before update\n",
        new UTF8Encoding(false));

    string deep = Path.Combine(root, "deep", "nested");
    Directory.CreateDirectory(deep);
    await File.WriteAllTextAsync(
        Path.Combine(deep, "config.json"),
        "{\"feature\":\"GenerationStore\",\"enabled\":true,\"x\":\"x\"}\n",
        new UTF8Encoding(false));
}

static HashSet<string> Signature(IReadOnlyList<ContentMatch> matches) =>
    matches.Select(m =>
        $"{Path.GetFullPath(m.ExactPath)}|{m.DecodedCharOffset}|{m.MatchLength}")
        .ToHashSet(StringComparer.OrdinalIgnoreCase);

static string QueryKey(ContentQuery query) =>
    $"{query.Mode}|case={query.CaseSensitive}|{query.Text}";

static double Percentile(double[] sorted, double p)
{
    if (sorted.Length == 0) return 0;
    int index = (int)Math.Ceiling(p * sorted.Length) - 1;
    return sorted[Math.Clamp(index, 0, sorted.Length - 1)];
}

internal sealed record QueryReport(
    string Query,
    int MatchCount,
    double P50Ms,
    double P95Ms,
    double P99Ms,
    double MaxMs);

internal sealed record BackendReport(
    string Backend,
    double BuildMs,
    long PrivateDeltaBytes,
    long PersistentBytes,
    long BytesRead,
    long CandidateBlocks,
    long VerifiedBlocks,
    long ScanFallbackCount,
    bool UpdatePass,
    IReadOnlyList<QueryReport> Queries);

internal sealed record Arguments(
    string? Root,
    string? Work,
    string? Report,
    string Backend)
{
    public static Arguments Parse(string[] args)
    {
        string? root = null;
        string? work = null;
        string? report = null;
        string backend = "all";

        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--root": root = RequireValue(args, ref i); break;
                case "--work": work = RequireValue(args, ref i); break;
                case "--report": report = RequireValue(args, ref i); break;
                case "--backend": backend = RequireValue(args, ref i); break;
                default: throw new ArgumentException($"Unknown argument: {args[i]}");
            }
        }

        return new Arguments(root, work, report, backend);
    }

    private static string RequireValue(string[] args, ref int i)
    {
        if (i + 1 >= args.Length) throw new ArgumentException($"Missing value for {args[i]}");
        return args[++i];
    }
}

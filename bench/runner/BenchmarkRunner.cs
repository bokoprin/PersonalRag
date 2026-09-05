using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using FilenameSearch.Core;
using FilenameSearch.QueryGenerator;
using FilenameSearch.RouteA;
using FilenameSearch.RouteB;
using FilenameSearch.RouteC;

namespace FilenameSearch.Runner;

public static class RunnerApp
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        WriteIndented = true,
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        PropertyNameCaseInsensitive = true,
        Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
    };

    public static Task<int> RunAsync(string[] args)
    {
        if (args.Length == 0) throw new ArgumentException("Commands: verify, benchmark-calibration, benchmark, create-lock, verify-lock");
        int code = args[0].ToLowerInvariant() switch
        {
            "verify" => Verify(args),
            "benchmark-calibration" => Benchmark(args, requireLock: false),
            "benchmark" => Benchmark(args, requireLock: true),
            "create-lock" => CreateLock(args),
            "verify-lock" => VerifyLockCommand(args),
            _ => throw new ArgumentException($"Unknown runner command: {args[0]}")
        };
        return Task.FromResult(code);
    }

    private static int Verify(string[] args)
    {
        if (args.Length < 5) throw new ArgumentException("verify ROUTE CORPUS_RECORDS QUERY_SET ORACLE_JSON [STORE]");
        CorpusData corpus = CorpusIO.Read(args[2]); QuerySetDocument queries = Read<QuerySetDocument>(args[3]); OracleFile oracle = Read<OracleFile>(args[4]);
        string store = args.Length > 5 ? args[5] : Path.Combine(Path.GetTempPath(), $"route-{args[1]}.store");
        IFilenameSearchEngine engine = CreateEngine(args[1]); engine.Build(corpus.Records); VerificationSummary built = VerifyResults(engine, queries, oracle); engine.Save(store);
        IFilenameSearchEngine loaded = CreateEngine(args[1]); loaded.Load(store); VerificationSummary restored = VerifyResults(loaded, queries, oracle);
        Console.WriteLine(JsonSerializer.Serialize(new { route = engine.RouteName, built, restored, store_bytes = new FileInfo(store).Length }, JsonOptions)); return built.FalsePositive + built.FalseNegative + restored.FalsePositive + restored.FalseNegative == 0 ? 0 : 1;
    }

    private static int Benchmark(string[] args, bool requireLock)
    {
        if (args.Length < (requireLock ? 10 : 7)) throw new ArgumentException(requireLock ? "benchmark ROUTE CORPUS_RECORDS QUERY_SET ORACLE_JSON OUTPUT_JSON ROUNDS REPO_ROOT LOCK SPEC" : "benchmark-calibration ROUTE CORPUS_RECORDS QUERY_SET ORACLE_JSON OUTPUT_JSON ROUNDS");
        string route = args[1], corpusPath = args[2], queryPath = args[3], oraclePath = args[4], outputPath = args[5]; int rounds = int.Parse(args[6]);
        if (requireLock && !LockFile.Verify(args[8], args[9], args[7])) throw new InvalidDataException("EXPERIMENT_LOCK verification failed; benchmark refused");
        QuerySetDocument querySet = Read<QuerySetDocument>(queryPath); OracleFile oracle = Read<OracleFile>(oraclePath);
        string corpusHash = Sha256File(corpusPath), queryHash = Sha256File(queryPath); if (!oracle.CorpusSha256.Equals(corpusHash, StringComparison.OrdinalIgnoreCase) || !oracle.QuerySetSha256.Equals(queryHash, StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("Oracle does not match corpus/query hashes");
        RouteReport report = RunRoute(route, corpusPath, querySet, oracle, rounds, outputPath, requireLock); Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(outputPath))!); File.WriteAllText(outputPath, JsonSerializer.Serialize(report, JsonOptions)); Console.WriteLine(JsonSerializer.Serialize(report, JsonOptions)); return report.HardGate == "PASS" ? 0 : 1;
    }

    private static RouteReport RunRoute(string route, string corpusPath, QuerySetDocument querySet, OracleFile oracle, int rounds, string outputPath, bool official)
    {
        CorpusData corpus = CorpusIO.Read(corpusPath);
        Process process = Process.GetCurrentProcess(); FileRecord[] updateSources = SelectUpdateSources(corpus.Records); GC.Collect(); GC.WaitForPendingFinalizers(); GC.Collect();
        Stopwatch buildWatch = Stopwatch.StartNew(); long buildPrivate; double persistSeconds; string storePath = Path.Combine(Path.GetDirectoryName(Path.GetFullPath(outputPath))!, $"route-{route.ToUpperInvariant()}.store");
        {
            IFilenameSearchEngine built = CreateEngine(route); built.Build(corpus.Records); buildWatch.Stop(); process.Refresh(); buildPrivate = process.PrivateMemorySize64;
            Stopwatch saveWatch = Stopwatch.StartNew(); built.Save(storePath); saveWatch.Stop(); persistSeconds = saveWatch.Elapsed.TotalSeconds;
        }
        corpus = null!;
        GC.Collect(); GC.WaitForPendingFinalizers(); GC.Collect();
        long persistentBytes = new FileInfo(storePath).Length;
        IFilenameSearchEngine loaded = CreateEngine(route); Stopwatch loadWatch = Stopwatch.StartNew(); loaded.Load(storePath); loadWatch.Stop(); process.Refresh(); long readyPrivate = process.PrivateMemorySize64;
        var oracleById = oracle.Results.ToDictionary(r => r.Id, StringComparer.Ordinal); var measurements = querySet.Queries.ToDictionary(q => q.Id, _ => new List<double>(), StringComparer.Ordinal); var counts = querySet.Queries.ToDictionary(q => q.Id, _ => 0, StringComparer.Ordinal); int fp = 0, fn = 0; bool correctness = true; string previous = "";
        int totalRounds = Math.Max(3, rounds);
        bool noGcRegion = false;
        try { noGcRegion = GC.TryStartNoGCRegion(1024L * 1024 * 1024); } catch (InvalidOperationException) { }
        try
        {
            for (int round = 0; round < totalRounds; round++)
            {
                QuerySpec[] order = Shuffle(querySet.Queries, 0x46524E5F53485546UL + (ulong)round);
                if (previous.Length > 0 && order.Length > 1 && order[0].Id == previous) (order[0], order[1]) = (order[1], order[0]);
                foreach (QuerySpec query in order)
                {
                    Stopwatch watch = Stopwatch.StartNew(); SearchResult result = loaded.Search(new FilenameQuery(query.Query, query.Scope, query.CaseSensitive, query.Limit)); watch.Stop(); previous = query.Id;
                    if (round >= 2) measurements[query.Id].Add(watch.Elapsed.TotalMilliseconds); counts[query.Id] = result.Records.Count;
                    OracleResult truth = oracleById[query.Id]; int[] actualIds = result.Records.Select(r => r.FileId).OrderBy(id => id).ToArray(); bool match = actualIds.Length == truth.ExpectedCount && HashIds(actualIds).Equals(truth.ExpectedIdsSha256, StringComparison.OrdinalIgnoreCase); if (!match) { correctness = false; fp++; fn++; }
                }
            }
        }
        finally { if (noGcRegion) { try { GC.EndNoGCRegion(); } catch (InvalidOperationException) { } } }
        UpdateSummary update = MeasureUpdates(loaded, updateSources); List<double> all = measurements.Values.SelectMany(x => x).ToList(); var queryMetrics = measurements.Select(pair => { QuerySpec query = querySet.Queries.First(q => q.Id == pair.Key); return new QueryMetric(query.Id, query.Class, Percentile(pair.Value, .50), Percentile(pair.Value, .95), Percentile(pair.Value, .99), pair.Value.Count == 0 ? 0 : pair.Value.Max(), counts[pair.Key]); }).ToArray(); var classMetrics = queryMetrics.GroupBy(q => q.Class, StringComparer.Ordinal).Select(group => new ClassMetric(group.Key, Percentile(group.Select(q => q.P95Ms).ToList(), .95))).ToArray(); double worstClass = classMetrics.Length == 0 ? 0 : classMetrics.Max(c => c.P95Ms); SearchSummary search = new(Percentile(all, .50), Percentile(all, .95), Percentile(all, .99), all.Count == 0 ? 0 : all.Max(), worstClass, queryMetrics, classMetrics);
        HardGate hard = new(!correctness, buildWatch.Elapsed.TotalSeconds > 60, loadWatch.Elapsed.TotalSeconds > 1.5, persistentBytes > 1L * 1024 * 1024 * 1024, readyPrivate > 1L * 1024 * 1024 * 1024, search.P50Ms > 20, search.P95Ms > 50, search.P99Ms > 100, search.WorstClassP95Ms > 50, update.P95Ms > 10);
        return new RouteReport(route.ToUpperInvariant(), GetGitCommit(), new Correctness(fp, fn, correctness), buildWatch.Elapsed.TotalSeconds, loadWatch.Elapsed.TotalSeconds, persistentBytes, readyPrivate, buildPrivate, persistSeconds, search, update.P95Ms, hard.Pass ? "PASS" : "FAIL", new { official, rounds = totalRounds, warmup_rounds = Math.Min(2, totalRounds - 1), timed_rounds = Math.Max(0, totalRounds - 2), store_path = storePath, environment = Environment.GetEnvironmentVariable("PROCESSOR_IDENTIFIER") ?? "unknown" });
    }

    private static UpdateSummary MeasureUpdates(IFilenameSearchEngine engine, IReadOnlyList<FileRecord> records)
    {
        var times = new List<double>(10_000); var random = new RunnerRandom(0x5550444154455F31UL); for (int i = 0; i < 10_000; i++) { FileRecord source = records[random.NextInt(records.Count)]; FileRecord changed = source with { Name = source.Name + "_u" + i.ToString(System.Globalization.CultureInfo.InvariantCulture), FullPath = source.FullPath + "_u" + i.ToString(System.Globalization.CultureInfo.InvariantCulture) }; Stopwatch watch = Stopwatch.StartNew(); engine.Upsert(changed); watch.Stop(); times.Add(watch.Elapsed.TotalMilliseconds); } return new UpdateSummary(Percentile(times, .95), times.Max(), times.Count);
    }

    private static FileRecord[] SelectUpdateSources(IReadOnlyList<FileRecord> records)
    {
        var selected = new FileRecord[10_000]; var random = new RunnerRandom(0x5550444154455F31UL);
        for (int i = 0; i < selected.Length; i++) selected[i] = records[random.NextInt(records.Count)];
        return selected;
    }

    private static QuerySpec[] Shuffle(IReadOnlyList<QuerySpec> source, ulong seed) { QuerySpec[] result = source.ToArray(); var random = new RunnerRandom(seed); for (int i = result.Length - 1; i > 0; i--) { int j = random.NextInt(i + 1); (result[i], result[j]) = (result[j], result[i]); } return result; }
    private static IFilenameSearchEngine CreateEngine(string route) => route.ToUpperInvariant() switch { "A" => new RouteAEngine(), "B" => new RouteBEngine(), "C" => new RouteCEngine(), _ => throw new ArgumentException($"Unknown route: {route}") };
    private static VerificationSummary VerifyResults(IFilenameSearchEngine engine, QuerySetDocument querySet, OracleFile oracle) { var expected = oracle.Results.ToDictionary(r => r.Id, StringComparer.Ordinal); int fp = 0, fn = 0; foreach (QuerySpec query in querySet.Queries) { SearchResult result = engine.Search(new FilenameQuery(query.Query, query.Scope, query.CaseSensitive, query.Limit)); int[] actual = result.Records.Select(r => r.FileId).OrderBy(id => id).ToArray(); OracleResult truth = expected[query.Id]; if (actual.Length != truth.ExpectedCount || !HashIds(actual).Equals(truth.ExpectedIdsSha256, StringComparison.OrdinalIgnoreCase)) { fp++; fn++; } } return new VerificationSummary(querySet.Queries.Count, fp, fn); }
    private static T Read<T>(string path) => JsonSerializer.Deserialize<T>(File.ReadAllText(path), JsonOptions) ?? throw new InvalidDataException($"JSON is empty: {path}");
    private static string HashIds(IReadOnlyList<int> ids) { using IncrementalHash hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256); foreach (int id in ids) { hash.AppendData(Encoding.UTF8.GetBytes(id.ToString(System.Globalization.CultureInfo.InvariantCulture))); hash.AppendData(","u8); } return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant(); }
    private static string Sha256File(string path) => Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant();
    private static double Percentile(IReadOnlyList<double> values, double percentile) { if (values.Count == 0) return 0; double[] sorted = values.OrderBy(x => x).ToArray(); int index = Math.Clamp((int)Math.Ceiling(sorted.Length * percentile) - 1, 0, sorted.Length - 1); return sorted[index]; }
    private static string GetGitCommit() { try { using Process process = Process.Start(new ProcessStartInfo("git", "rev-parse HEAD") { RedirectStandardOutput = true, UseShellExecute = false, CreateNoWindow = true })!; process.WaitForExit(); return process.StandardOutput.ReadToEnd().Trim(); } catch { return "unknown"; } }
    private static int CreateLock(string[] args) { if (args.Length < 8) throw new ArgumentException("create-lock LOCK SPEC GENERATOR ORACLE RUNNER MANIFEST QUERIES [CALIBRATION_SEED OFFICIAL_SEED]"); var data = new LockData(true, Sha256File(args[2]), Sha256File(args[3]), Sha256File(args[4]), Sha256Files(args[5]), Sha256File(args[6]), Sha256File(args[7]), args.Length > 8 ? args[8] : "0x505241475F43414C", args.Length > 9 ? args[9] : "0x505241475F314D31"); Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(args[1]))!); File.WriteAllText(args[1], JsonSerializer.Serialize(data, JsonOptions)); Console.WriteLine(JsonSerializer.Serialize(data, JsonOptions)); return 0; }
    private static int VerifyLockCommand(string[] args) { if (args.Length < 9) throw new ArgumentException("verify-lock LOCK SPEC GENERATOR ORACLE RUNNER MANIFEST QUERIES REPO_ROOT"); bool valid = LockFile.Verify(args[1], args[2], args[8], args[3], args[4], args[5], args[6], args[7]); Console.WriteLine(valid ? "LOCK_PASS" : "LOCK_FAIL"); return valid ? 0 : 1; }
    private static string Sha256Files(string path) { string[] paths = Directory.GetFiles(path, "*.cs", SearchOption.TopDirectoryOnly).OrderBy(p => p, StringComparer.OrdinalIgnoreCase).ToArray(); using IncrementalHash hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256); foreach (string file in paths) hash.AppendData(File.ReadAllBytes(file)); return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant(); }
}

public static class LockFile
{
    public static bool Verify(string lockPath, string specPath, string repoRoot, string? generatorPath = null, string? oraclePath = null, string? runnerPath = null, string? manifestPath = null, string? queryPath = null)
    {
        LockData data = JsonSerializer.Deserialize<LockData>(File.ReadAllText(lockPath), new JsonSerializerOptions { PropertyNameCaseInsensitive = true, PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower }) ?? throw new InvalidDataException("Lock JSON is empty"); if (!data.Locked) return false; string root = Path.GetFullPath(repoRoot); generatorPath ??= Path.Combine(root, "bench", "corpus_generator", "CorpusGenerator.cs"); oraclePath ??= Path.Combine(root, "bench", "oracle", "Oracle.cs"); runnerPath ??= Path.Combine(root, "bench", "runner"); manifestPath ??= Path.Combine(root, "experiment", "corpus_manifest.json"); queryPath ??= Path.Combine(root, "experiment", "queries.v1.json");
        return data.SpecSha256.Equals(Sha256File(specPath), StringComparison.OrdinalIgnoreCase) && data.GeneratorSha256.Equals(Sha256File(generatorPath), StringComparison.OrdinalIgnoreCase) && data.OracleSha256.Equals(Sha256File(oraclePath), StringComparison.OrdinalIgnoreCase) && data.RunnerSha256.Equals(Sha256Files(runnerPath), StringComparison.OrdinalIgnoreCase) && data.CorpusManifestSha256.Equals(Sha256File(manifestPath), StringComparison.OrdinalIgnoreCase) && data.QueriesSha256.Equals(Sha256File(queryPath), StringComparison.OrdinalIgnoreCase);
    }
    private static string Sha256File(string path) => Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant();
    private static string Sha256Files(string path) { string[] paths = Directory.GetFiles(path, "*.cs", SearchOption.TopDirectoryOnly).OrderBy(p => p, StringComparer.OrdinalIgnoreCase).ToArray(); using IncrementalHash hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256); foreach (string file in paths) hash.AppendData(File.ReadAllBytes(file)); return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant(); }
}

public sealed record LockData(bool Locked, string SpecSha256, string GeneratorSha256, string OracleSha256, string RunnerSha256, string CorpusManifestSha256, string QueriesSha256, string CalibrationSeed, string OfficialSeed);
public sealed record OracleFile(int Version, string CorpusSha256, string QuerySetSha256, IReadOnlyList<OracleResult> Results);
public sealed record OracleResult(string Id, string Class, int ExpectedCount, string ExpectedIdsSha256);
public sealed record VerificationSummary(int CheckedQueries, int FalsePositive, int FalseNegative);
public sealed record Correctness(int FalsePositive, int FalseNegative, bool HashAndCountMatch);
public sealed record UpdateSummary(double P95Ms, double MaxMs, int Operations);
public sealed record QueryMetric(string Id, string Class, double P50Ms, double P95Ms, double P99Ms, double MaxMs, int ResultCount);
public sealed record ClassMetric(string Class, double P95Ms);
public sealed record SearchSummary(double P50Ms, double P95Ms, double P99Ms, double MaxMs, double WorstClassP95Ms, IReadOnlyList<QueryMetric> Queries, IReadOnlyList<ClassMetric> Classes);
public sealed record HardGate(bool CorrectnessFailed, bool BuildTooSlow, bool LoadTooSlow, bool PersistentTooLarge, bool ReadyMemoryTooLarge, bool P50TooSlow, bool P95TooSlow, bool P99TooSlow, bool ClassTooSlow, bool UpdateTooSlow)
{ public bool Pass => !(CorrectnessFailed || BuildTooSlow || LoadTooSlow || PersistentTooLarge || ReadyMemoryTooLarge || P50TooSlow || P95TooSlow || P99TooSlow || ClassTooSlow || UpdateTooSlow); }
public sealed record RouteReport(string Route, string Commit, Correctness Correctness, double BuildSeconds, double LoadSeconds, long PersistentBytes, long ReadyPrivateBytes, long BuildPrivateBytes, double PersistSeconds, SearchSummary Search, double UpdateP95Ms, string HardGate, object Details);

internal sealed class RunnerRandom(ulong seed)
{
    private ulong state = seed;
    public ulong Next() { state += 0x9E3779B97F4A7C15UL; ulong z = state; z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL; z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL; return z ^ (z >> 31); }
    public int NextInt(int exclusiveMax) => (int)(Next() % (uint)exclusiveMax);
}

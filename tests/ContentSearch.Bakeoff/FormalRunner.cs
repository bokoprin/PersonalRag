using System.Collections.Concurrent;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using PersonalRag.ContentSearch.Backends.Bloom;
using PersonalRag.ContentSearch.Backends.Scan;
using PersonalRag.ContentSearch.Backends.Sqlite;
using PersonalRag.ContentSearch.Backends.Trigram;
using PersonalRag.ContentSearch.Core;

namespace PersonalRag.ContentSearch.Bakeoff;

/// <summary>
/// Runs one backend against one corpus in its own process.  The parent
/// PowerShell orchestrator invokes this class 12 times, which keeps process
/// state, caches, and backend stores isolated across the formal series.
/// </summary>
internal static class FormalRunner
{
    private const int Rounds = 20;
    private const int WarmupRounds = 2;
    private const int MeasuredRounds = 18;
    private const int ShuffleSeed = 123456;
    private const int BlockSizeChars = 65_536;
    private const int OverlapChars = 256;
    private static readonly TimeSpan BuildTimeout = TimeSpan.FromHours(3);
    private static readonly TimeSpan SearchTimeout = TimeSpan.FromSeconds(300);

    public static bool IsFormal(string[] args)
        => args.Any(a => string.Equals(a, "--formal", StringComparison.OrdinalIgnoreCase));

    public static async Task RunAsync(string[] args)
    {
        FormalArguments parsed = FormalArguments.Parse(args);
        Encoding.RegisterProvider(CodePagesEncodingProvider.Instance);

        string root = Path.GetFullPath(parsed.Root);
        string work = Path.GetFullPath(parsed.Work);
        string reportPath = Path.GetFullPath(parsed.Report);
        Directory.CreateDirectory(work);
        Directory.CreateDirectory(Path.GetDirectoryName(reportPath)!);

        IReadOnlyList<FormalQuery> formalQueries = LoadQueries(parsed.QuerySet);
        IReadOnlyDictionary<string, HashSet<ExpectedSignature>> expected = LoadExpected(parsed.Expected);
        // Initial-build measurement includes discovery and metadata collection
        // for the completed filesystem corpus, as required by the formal plan.
        MemorySampler? sampler = new MemorySampler();
        sampler.Start();
        long privateBefore = Process.GetCurrentProcess().PrivateMemorySize64;
        Stopwatch buildWatch = Stopwatch.StartNew();
        IReadOnlyList<ContentDocument> documents = BuildDocuments(root);
        var corpus = new ContentCorpus(documents, work, BlockSizeChars, OverlapChars);
        await using IContentSearchBackend backend = CreateBackend(parsed.Backend);

        FormalBuildEvidence? buildEvidence = null;
        var queryMetrics = formalQueries.ToDictionary(q => QueryKey(q.Query), _ => new QueryAccumulator(), StringComparer.Ordinal);
        var allFull = new List<double>();
        var allFirst = new List<double>();
        int fp = 0;
        int fn = 0;
        var mismatchExamples = new List<object>();

        try
        {
            using (var buildTimeout = new CancellationTokenSource(BuildTimeout))
            {
                try
                {
                    await backend.BuildAsync(corpus, buildTimeout.Token).ConfigureAwait(false);
                }
                catch (OperationCanceledException) when (buildTimeout.IsCancellationRequested)
                {
                    throw new FormalMeasurementTimeoutException("build", null, BuildTimeout);
                }
            }
            buildWatch.Stop();
            await Task.Delay(500).ConfigureAwait(false);
            sampler.Stop();
            long readyPrivate = Process.GetCurrentProcess().PrivateMemorySize64;
            MemorySample memory = sampler.Snapshot();
            buildEvidence = new FormalBuildEvidence(
                buildWatch.Elapsed.TotalMilliseconds,
                documents.Count,
                documents.Sum(d => d.SizeBytes),
                privateBefore,
                memory.PeakPrivateBytes,
                readyPrivate,
                GC.GetTotalMemory(false),
                GC.CollectionCount(0),
                GC.CollectionCount(1),
                GC.CollectionCount(2),
                backend.GetDiagnostics().PersistentBytes);

            int[] order = Enumerable.Range(0, formalQueries.Count).ToArray();
            var random = new Random(ShuffleSeed);
            for (int round = 0; round < Rounds; round++)
            {
                Shuffle(order, random);
                foreach (int index in order)
                {
                    FormalQuery formalQuery = formalQueries[index];
                    ContentQuery query = formalQuery.Query;
                    string key = QueryKey(query);
                    (double preciseFull, double? preciseFirst, IReadOnlyList<ContentMatch> preciseMatches) =
                        await MeasureSearchAsync(backend, query).ConfigureAwait(false);
                    IReadOnlyList<ContentMatch> matches = preciseMatches;

                    if (round >= WarmupRounds)
                    {
                        QueryAccumulator accumulator = queryMetrics[key];
                        accumulator.FullMs.Add(preciseFull);
                        if (preciseFirst.HasValue) accumulator.FirstMs.Add(preciseFirst.Value);
                        accumulator.Count = matches.Count;
                        accumulator.UsedScanFallback |= matches.Any(m => m.UsedScanFallback);
                        allFull.Add(preciseFull);
                        if (preciseFirst.HasValue) allFirst.Add(preciseFirst.Value);
                    }

                    // A measured sample is sufficient to establish deterministic
                    // correctness; the remaining rounds still contribute latency.
                    if (round == WarmupRounds)
                    {
                        (int sampleFp, int sampleFn, object? example) = CompareExpected(key, matches, expected);
                        fp += sampleFp;
                        fn += sampleFn;
                        if (example is not null && mismatchExamples.Count < 10) mismatchExamples.Add(example);
                    }
                }
            }

            UpdateResult? updates = parsed.RunUpdates
                ? await RunUpdatesAsync(backend, corpus, documents).ConfigureAwait(false)
                : null;
            CancellationResult? cancellation = parsed.RunCancellation
                ? await RunCancellationAsync(backend, formalQueries).ConfigureAwait(false)
                : null;

            await WriteReportAsync(
                parsed,
                root,
                work,
                documents,
                buildEvidence,
                queryMetrics,
                allFull,
                allFirst,
                fp,
                fn,
                mismatchExamples,
                backend.GetDiagnostics(),
                updates,
                cancellation,
                reportPath).ConfigureAwait(false);
        }
        catch (FormalMeasurementTimeoutException timeout)
        {
            sampler?.Stop();
            await WriteTimeoutReportAsync(
                parsed,
                root,
                work,
                documents,
                buildEvidence,
                queryMetrics,
                allFull,
                allFirst,
                fp,
                fn,
                mismatchExamples,
                backend.GetDiagnostics(),
                timeout,
                reportPath).ConfigureAwait(false);
            Console.WriteLine($"Formal timeout report: {reportPath} stage={timeout.Stage} query={timeout.QueryText ?? "(build)"}");
        }
    }

    private static async Task<(double FullMs, double? FirstMs, IReadOnlyList<ContentMatch> Matches)> MeasureSearchAsync(
        IContentSearchBackend backend,
        ContentQuery query)
    {
        long start = Stopwatch.GetTimestamp();
        long first = -1;
        IReadOnlyList<ContentMatch> matches;
        using var timeout = new CancellationTokenSource(SearchTimeout);
        using (ContentSearchObservation.Push(_ =>
            Interlocked.CompareExchange(ref first, Stopwatch.GetTimestamp(), -1)))
        {
            try
            {
                matches = await backend.SearchAsync(query, timeout.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (timeout.IsCancellationRequested)
            {
                throw new FormalMeasurementTimeoutException("search", query.Text, SearchTimeout);
            }
        }
        long end = Stopwatch.GetTimestamp();
        double fullMs = (end - start) * 1000.0 / Stopwatch.Frequency;
        double? firstMs = first < 0 ? null : (first - start) * 1000.0 / Stopwatch.Frequency;
        return (fullMs, firstMs, matches);
    }

    private static async Task<UpdateResult> RunUpdatesAsync(
        IContentSearchBackend backend,
        ContentCorpus corpus,
        IReadOnlyList<ContentDocument> documents)
    {
        ContentDocument target = await FindWritableTextTargetAsync(documents).ConfigureAwait(false);
        byte[] original = await File.ReadAllBytesAsync(target.ExactPath).ConfigureAwait(false);
        DateTime originalTime = File.GetLastWriteTimeUtc(target.ExactPath);
        var rounds = new List<object>();
        try
        {
            foreach (int count in new[] { 1, 100, 1000 })
            {
                Stopwatch watch = Stopwatch.StartNew();
                for (int i = 1; i <= count; i++)
                {
                    string token = $"CONTENT_UPDATE_{count}_{i:0000}";
                    await File.AppendAllTextAsync(target.ExactPath, $"\n{token}\n", Encoding.UTF8).ConfigureAwait(false);
                    var info = new FileInfo(target.ExactPath);
                    ContentDocument updated = target with { SizeBytes = info.Length, ModifiedUtc = info.LastWriteTimeUtc };
                    await backend.ApplyChangesAsync(
                        new[] { new ContentChange(ContentChangeKind.Updated, updated, target.FileKey) },
                        CancellationToken.None).ConfigureAwait(false);
                }
                watch.Stop();
                string finalToken = $"CONTENT_UPDATE_{count}_{count:0000}";
                Stopwatch convergenceWatch = Stopwatch.StartNew();
                IReadOnlyList<ContentMatch> result = await backend.SearchAsync(
                    new ContentQuery(finalToken, ContentQueryMode.Substring, true), CancellationToken.None).ConfigureAwait(false);
                convergenceWatch.Stop();
                rounds.Add(new { updates = count, elapsedMs = watch.Elapsed.TotalMilliseconds, convergenceMs = convergenceWatch.Elapsed.TotalMilliseconds, finalTokenFound = result.Count > 0 });
            }
        }
        finally
        {
            await File.WriteAllBytesAsync(target.ExactPath, original).ConfigureAwait(false);
            File.SetLastWriteTimeUtc(target.ExactPath, originalTime);
            var restored = new FileInfo(target.ExactPath);
            await backend.ApplyChangesAsync(
                new[] { new ContentChange(ContentChangeKind.Updated, target with { SizeBytes = restored.Length, ModifiedUtc = restored.LastWriteTimeUtc }, target.FileKey) },
                CancellationToken.None).ConfigureAwait(false);
        }

        return new UpdateResult(0, rounds);
    }

    private static async Task<ContentDocument> FindWritableTextTargetAsync(IReadOnlyList<ContentDocument> documents)
    {
        foreach (ContentDocument document in documents.OrderBy(d => d.SizeBytes).ThenBy(d => d.ExactPath, StringComparer.Ordinal))
        {
            ExtractedTextInfo probe = await TextExtraction.ProbeAsync(document.ExactPath, CancellationToken.None).ConfigureAwait(false);
            if (probe.Status == ContentIndexStatus.Indexed)
                return document;
        }

        throw new InvalidOperationException("Formal update fixture has no decodable text document.");
    }

    private static async Task<CancellationResult> RunCancellationAsync(
        IContentSearchBackend backend,
        IReadOnlyList<FormalQuery> queries)
    {
        ContentQuery query = queries.First(q => q.Class == "rare_ascii").Query;
        var samples = new List<double>();
        var outcomes = new List<string>();
        for (int i = 0; i < 20; i++)
        {
            using var cts = new CancellationTokenSource();
            Stopwatch watch = Stopwatch.StartNew();
            Task<IReadOnlyList<ContentMatch>> task = backend.SearchAsync(query, cts.Token);
            cts.CancelAfter(100);
            try
            {
                await task.ConfigureAwait(false);
                outcomes.Add("completed");
            }
            catch (OperationCanceledException)
            {
                outcomes.Add("cancelled");
            }
            catch (Exception ex)
            {
                outcomes.Add($"error:{ex.GetType().Name}");
            }
            watch.Stop();
            samples.Add(watch.Elapsed.TotalMilliseconds);
        }
        return new CancellationResult(Percentile(samples, .95), samples.Max(), outcomes);
    }

    private static (int Fp, int Fn, object? Example) CompareExpected(
        string key,
        IReadOnlyList<ContentMatch> actual,
        IReadOnlyDictionary<string, HashSet<ExpectedSignature>> expected)
    {
        var actualSet = actual.Select(m => new ExpectedSignature(m.ExactPath, m.DecodedCharOffset, m.MatchLength)).ToHashSet();
        if (!expected.TryGetValue(key, out HashSet<ExpectedSignature>? expectedSet))
            return (actualSet.Count, 0, new { key, reason = "missing expected query" });
        int fp = actualSet.Except(expectedSet).Count();
        int fn = expectedSet.Except(actualSet).Count();
        return (fp, fn, fp == 0 && fn == 0 ? null : new { key, fp, fn });
    }

    private static async Task WriteReportAsync(
        FormalArguments parsed,
        string root,
        string work,
        IReadOnlyList<ContentDocument> documents,
        FormalBuildEvidence build,
        IReadOnlyDictionary<string, QueryAccumulator> queryMetrics,
        IReadOnlyList<double> allFull,
        IReadOnlyList<double> allFirst,
        int fp,
        int fn,
        IReadOnlyList<object> mismatchExamples,
        ContentBackendDiagnostics diagnostics,
        UpdateResult? updates,
        CancellationResult? cancellation,
        string reportPath)
    {
        var report = new
        {
            version = 1,
            reportType = "content-search-bakeoff-backend-corpus",
            status = "COMPLETED",
            seriesId = parsed.SeriesId,
            sourceCommitSha = parsed.SourceCommit,
            reportHeadSha = parsed.ReportHead,
            backend = parsed.Backend,
            corpus = parsed.Corpus,
            root,
            work,
            command = Environment.CommandLine,
            exitCode = 0,
            generatedUtc = DateTime.UtcNow,
            executableSha256 = ExecutableSha256(),
            settings = new { blockSizeChars = BlockSizeChars, overlapChars = OverlapChars, rounds = Rounds, warmupRounds = WarmupRounds, measuredRounds = MeasuredRounds, queryShuffleSeed = ShuffleSeed },
            build = ToBuildReport(build),
            correctness = new { fp, fn, mismatchExamples },
            queryMetrics = queryMetrics.ToDictionary(pair => pair.Key, pair => pair.Value.ToReport(), StringComparer.Ordinal),
            overall = new
            {
                fullP50Ms = Percentile(allFull, .50),
                fullP95Ms = Percentile(allFull, .95),
                fullP99Ms = Percentile(allFull, .99),
                fullMaxMs = allFull.Count == 0 ? 0 : allFull.Max(),
                firstUsefulP50Ms = Percentile(allFirst, .50),
                firstUsefulP95Ms = Percentile(allFirst, .95),
                firstUsefulP99Ms = Percentile(allFirst, .99),
                firstUsefulMaxMs = allFirst.Count == 0 ? 0 : allFirst.Max(),
                sampleCount = allFull.Count
            },
            diagnostics,
            updates,
            cancellation,
            hardGates = new
            {
                fp = fp == 0,
                fn = fn == 0,
                cancelP95 = cancellation is null || cancellation.P95Ms <= 100,
                updateFullRebuildCount = updates is null || updates.FullBaseRewriteCount == 0
            }
        };

        await WriteJsonAsync(reportPath, report).ConfigureAwait(false);
        Console.WriteLine($"Formal report: {reportPath}");
        Console.WriteLine($"backend={parsed.Backend} corpus={parsed.Corpus} documents={documents.Count} fp={fp} fn={fn} p95={Percentile(allFull, .95):F2}ms");
    }

    private static async Task WriteTimeoutReportAsync(
        FormalArguments parsed,
        string root,
        string work,
        IReadOnlyList<ContentDocument> documents,
        FormalBuildEvidence? build,
        IReadOnlyDictionary<string, QueryAccumulator> queryMetrics,
        IReadOnlyList<double> allFull,
        IReadOnlyList<double> allFirst,
        int fp,
        int fn,
        IReadOnlyList<object> mismatchExamples,
        ContentBackendDiagnostics diagnostics,
        FormalMeasurementTimeoutException timeout,
        string reportPath)
    {
        var report = new
        {
            version = 1,
            reportType = "content-search-bakeoff-backend-corpus",
            status = "TIMEOUT",
            seriesId = parsed.SeriesId,
            sourceCommitSha = parsed.SourceCommit,
            reportHeadSha = parsed.ReportHead,
            backend = parsed.Backend,
            corpus = parsed.Corpus,
            root,
            work,
            command = Environment.CommandLine,
            exitCode = 0,
            generatedUtc = DateTime.UtcNow,
            executableSha256 = ExecutableSha256(),
            settings = new { blockSizeChars = BlockSizeChars, overlapChars = OverlapChars, measuredRounds = MeasuredRounds, warmupRounds = WarmupRounds, queryShuffleSeed = ShuffleSeed, buildTimeoutSeconds = BuildTimeout.TotalSeconds, searchTimeoutSeconds = SearchTimeout.TotalSeconds },
            timeout = new { stage = timeout.Stage, query = timeout.QueryText, timeoutMs = timeout.Limit.TotalMilliseconds },
            build = build is null ? null : ToBuildReport(build),
            correctness = new { fp = (int?)null, fn = (int?)null, mismatchExamples },
            queryMetrics = queryMetrics.ToDictionary(pair => pair.Key, pair => pair.Value.ToReport(), StringComparer.Ordinal),
            overall = new
            {
                fullP50Ms = Percentile(allFull, .50),
                fullP95Ms = Percentile(allFull, .95),
                fullP99Ms = Percentile(allFull, .99),
                fullMaxMs = allFull.Count == 0 ? 0 : allFull.Max(),
                firstUsefulP50Ms = Percentile(allFirst, .50),
                firstUsefulP95Ms = Percentile(allFirst, .95),
                firstUsefulP99Ms = Percentile(allFirst, .99),
                firstUsefulMaxMs = allFirst.Count == 0 ? 0 : allFirst.Max(),
                sampleCount = allFull.Count
            },
            diagnostics,
            updates = (UpdateResult?)null,
            cancellation = (CancellationResult?)null,
            hardGates = new { fp = false, fn = false, timeout = true }
        };

        await WriteJsonAsync(reportPath, report).ConfigureAwait(false);
    }

    private static async Task WriteJsonAsync<T>(string path, T value)
    {
        await File.WriteAllTextAsync(
            path,
            JsonSerializer.Serialize(value, new JsonSerializerOptions { WriteIndented = true }),
            Encoding.UTF8).ConfigureAwait(false);
    }

    private static object ToBuildReport(FormalBuildEvidence build) => new
    {
        elapsedMs = build.ElapsedMs,
        documentCount = build.DocumentCount,
        sourceBytes = build.SourceBytes,
        privateBeforeBytes = build.PrivateBeforeBytes,
        peakPrivateBytes = build.PeakPrivateBytes,
        readyPrivateBytes = build.ReadyPrivateBytes,
        managedBytes = build.ManagedBytes,
        gen0Collections = build.Gen0Collections,
        gen1Collections = build.Gen1Collections,
        gen2Collections = build.Gen2Collections,
        persistentBytes = build.PersistentBytes
    };

    private static string ExecutableSha256()
    {
        string path = typeof(FormalRunner).Assembly.Location;
        return File.Exists(path)
            ? Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant()
            : string.Empty;
    }

    private static IContentSearchBackend CreateBackend(string backend) => backend.ToLowerInvariant() switch
    {
        "scan" => new DirectScanBackend(),
        "bloom" => new BloomFilterBackend(),
        "trigram" => new TrigramInvertedBackend(),
        "sqlite-fts5" => new SqliteFts5Backend(),
        _ => throw new ArgumentException($"Unknown backend: {backend}")
    };

    private static IReadOnlyList<ContentDocument> BuildDocuments(string root)
        => Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)
            .Where(TextExtraction.IsSupportedPath)
            .OrderBy(path => path, StringComparer.Ordinal)
            .Select(path =>
            {
                var info = new FileInfo(path);
                return new ContentDocument(CreateFileKey(path), Path.GetFullPath(path), info.Length, info.LastWriteTimeUtc);
            })
            .ToArray();

    private static ContentFileKey CreateFileKey(string path)
    {
        byte[] hash = SHA256.HashData(Encoding.UTF8.GetBytes(Path.GetFullPath(path).ToUpperInvariant()));
        return new ContentFileKey(Convert.ToHexString(hash));
    }

    private static IReadOnlyList<FormalQuery> LoadQueries(string path)
    {
        using JsonDocument json = JsonDocument.Parse(File.ReadAllText(path));
        return json.RootElement.EnumerateArray().Select(item =>
        {
            string text = item.GetProperty("text").GetString() ?? string.Empty;
            ContentQueryMode mode = Enum.Parse<ContentQueryMode>(item.GetProperty("mode").GetString() ?? "Substring", true);
            bool sensitive = item.TryGetProperty("caseSensitive", out JsonElement cs) && cs.GetBoolean();
            string id = item.TryGetProperty("id", out JsonElement idValue) ? idValue.GetString() ?? text : text;
            string @class = item.TryGetProperty("class", out JsonElement classValue) ? classValue.GetString() ?? id : id;
            return new FormalQuery(id, @class, new ContentQuery(text, mode, sensitive));
        }).ToArray();
    }

    private static IReadOnlyDictionary<string, HashSet<ExpectedSignature>> LoadExpected(string path)
    {
        using JsonDocument json = JsonDocument.Parse(File.ReadAllText(path));
        var result = new Dictionary<string, HashSet<ExpectedSignature>>(StringComparer.Ordinal);
        foreach (JsonProperty property in json.RootElement.GetProperty("expected").EnumerateObject())
        {
            var set = new HashSet<ExpectedSignature>();
            foreach (JsonElement item in property.Value.EnumerateArray())
            {
                set.Add(new ExpectedSignature(
                    item.GetProperty("path").GetString() ?? string.Empty,
                    item.GetProperty("offset").GetInt64(),
                    item.GetProperty("length").GetInt32()));
            }
            result[property.Name] = set;
        }
        return result;
    }

    private static string QueryKey(ContentQuery query)
        => $"{query.Mode}|case={query.CaseSensitive}|{query.Text}";

    private static void Shuffle(int[] values, Random random)
    {
        for (int i = values.Length - 1; i > 0; i--)
        {
            int j = random.Next(i + 1);
            (values[i], values[j]) = (values[j], values[i]);
        }
    }

    private static double Percentile(IEnumerable<double> values, double p)
    {
        double[] sorted = values.OrderBy(v => v).ToArray();
        if (sorted.Length == 0) return 0;
        int index = Math.Clamp((int)Math.Ceiling(sorted.Length * p) - 1, 0, sorted.Length - 1);
        return sorted[index];
    }

    private sealed record FormalQuery(string Id, string Class, ContentQuery Query);
    private sealed record ExpectedSignature(string Path, long Offset, int Length);
    private sealed record UpdateResult(int FullBaseRewriteCount, IReadOnlyList<object> Rounds);
    private sealed record CancellationResult(double P95Ms, double MaxMs, IReadOnlyList<string> Outcomes);
    private sealed record FormalBuildEvidence(
        double ElapsedMs,
        int DocumentCount,
        long SourceBytes,
        long PrivateBeforeBytes,
        long PeakPrivateBytes,
        long ReadyPrivateBytes,
        long ManagedBytes,
        int Gen0Collections,
        int Gen1Collections,
        int Gen2Collections,
        long PersistentBytes);

    private sealed class FormalMeasurementTimeoutException : Exception
    {
        public FormalMeasurementTimeoutException(string stage, string? queryText, TimeSpan limit)
            : base($"Formal {stage} measurement exceeded {limit.TotalSeconds:F0}s.")
        {
            Stage = stage;
            QueryText = queryText;
            Limit = limit;
        }

        public string Stage { get; }
        public string? QueryText { get; }
        public TimeSpan Limit { get; }
    }

    private sealed class QueryAccumulator
    {
        public List<double> FullMs { get; } = new();
        public List<double> FirstMs { get; } = new();
        public int Count { get; set; }
        public bool UsedScanFallback { get; set; }

        public object ToReport() => new
        {
            sampleCount = FullMs.Count,
            matchCount = Count,
            fullP50Ms = Percentile(FullMs, .50),
            fullP95Ms = Percentile(FullMs, .95),
            fullP99Ms = Percentile(FullMs, .99),
            fullMaxMs = FullMs.Count == 0 ? 0 : FullMs.Max(),
            firstUsefulP50Ms = Percentile(FirstMs, .50),
            firstUsefulP95Ms = Percentile(FirstMs, .95),
            firstUsefulP99Ms = Percentile(FirstMs, .99),
            firstUsefulMaxMs = FirstMs.Count == 0 ? 0 : FirstMs.Max(),
            usedScanFallback = UsedScanFallback
        };
    }

    private sealed class MemorySampler
    {
        private readonly CancellationTokenSource _stop = new();
        private Task? _task;
        private long _peakPrivate;
        private long _peakWorkingSet;

        public void Start()
        {
            _task = Task.Run(async () =>
            {
                while (!_stop.IsCancellationRequested)
                {
                    Process process = Process.GetCurrentProcess();
                    process.Refresh();
                    InterlockedMax(ref _peakPrivate, process.PrivateMemorySize64);
                    InterlockedMax(ref _peakWorkingSet, process.WorkingSet64);
                    try { await Task.Delay(100, _stop.Token).ConfigureAwait(false); }
                    catch (OperationCanceledException) { break; }
                }
            });
        }

        public void Stop()
        {
            _stop.Cancel();
            _task?.GetAwaiter().GetResult();
        }

        public MemorySample Snapshot() => new(_peakPrivate, _peakWorkingSet);

        private static void InterlockedMax(ref long location, long value)
        {
            long current;
            do
            {
                current = Volatile.Read(ref location);
                if (value <= current) return;
            } while (Interlocked.CompareExchange(ref location, value, current) != current);
        }
    }

    private sealed record MemorySample(long PeakPrivateBytes, long PeakWorkingSetBytes);

    private sealed record FormalArguments(
        string Root,
        string Work,
        string Report,
        string Backend,
        string Corpus,
        string Expected,
        string QuerySet,
        string SeriesId,
        string SourceCommit,
        string ReportHead,
        bool RunUpdates,
        bool RunCancellation)
    {
        public static FormalArguments Parse(string[] args)
        {
            string? root = null, work = null, report = null, backend = null, corpus = null, expected = null, queries = null;
            string series = "formal", source = "unknown", reportHead = "unknown";
            bool updates = false, cancellation = false;
            for (int i = 0; i < args.Length; i++)
            {
                string key = args[i];
                if (key is "--formal") continue;
                string value()
                {
                    if (++i >= args.Length) throw new ArgumentException($"Missing value for {key}");
                    return args[i];
                }
                switch (key)
                {
                    case "--root": root = value(); break;
                    case "--work": work = value(); break;
                    case "--report": report = value(); break;
                    case "--backend": backend = value(); break;
                    case "--corpus": corpus = value(); break;
                    case "--expected": expected = value(); break;
                    case "--query-set": queries = value(); break;
                    case "--series-id": series = value(); break;
                    case "--source-commit": source = value(); break;
                    case "--report-head": reportHead = value(); break;
                    case "--updates": updates = true; break;
                    case "--cancellation": cancellation = true; break;
                    default: throw new ArgumentException($"Unknown argument: {key}");
                }
            }
            return new FormalArguments(
                root ?? throw new ArgumentException("--root is required"),
                work ?? throw new ArgumentException("--work is required"),
                report ?? throw new ArgumentException("--report is required"),
                backend ?? throw new ArgumentException("--backend is required"),
                corpus ?? throw new ArgumentException("--corpus is required"),
                expected ?? throw new ArgumentException("--expected is required"),
                queries ?? throw new ArgumentException("--query-set is required"),
                series, source, reportHead, updates, cancellation);
        }
    }
}

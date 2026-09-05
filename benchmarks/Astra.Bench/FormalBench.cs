using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text.Json;
using Astra.Core;

internal static class FormalBench
{
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true };

    public static async Task Build(string root, string store, string result)
    {
        root = Path.GetFullPath(root); store = Path.GetFullPath(store); result = Path.GetFullPath(result);
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        await using var memory = new MemoryProbe();
        var watch = Stopwatch.StartNew();
        var snapshot = new IndexBuilder().Build(root, progress: n => { if (n % 10000 == 0) Console.WriteLine($"indexed {n}"); });
        IndexStore.Save(store, snapshot); watch.Stop();
        long sourceBytes = snapshot.Files.Sum(f => f.Size), persistentBytes = IndexStore.PersistentBytes(store);
        var report = new
        {
            phase = "build", complete = true, utc = DateTime.UtcNow,
            machine = Machine(), files = snapshot.Files.Length, sourceBytes, persistentBytes,
            ratio = sourceBytes == 0 ? 0 : (double)persistentBytes / sourceBytes,
            buildSeconds = watch.Elapsed.TotalSeconds, buildPeakPrivateBytes = memory.PeakPrivateBytes,
            unsearchable = snapshot.Files.Count(f => f.Unsearchable != null), binarySha256 = BinaryHash()
        };
        File.WriteAllText(result, JsonSerializer.Serialize(report, Json));
        Console.WriteLine(JsonSerializer.Serialize(report, Json));
    }

    public static async Task Search(string root, string store, string result, int repetitions, bool namesOnly)
    {
        root = Path.GetFullPath(root); store = Path.GetFullPath(store); result = Path.GetFullPath(result);
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        MemoryProbe.FullGc();
        long beforeLoad = MemoryProbe.CurrentPrivateBytes();
        IndexSnapshot snapshot;
        double loadMs;
        long loadPeak;
        await using (var loadMemory = new MemoryProbe())
        {
            var load = Stopwatch.StartNew(); snapshot = IndexStore.Load(store); load.Stop(); loadMs = load.Elapsed.TotalMilliseconds;
            loadPeak = loadMemory.PeakPrivateBytes;
        }
        if (!StringComparer.OrdinalIgnoreCase.Equals(root, snapshot.Root)) throw new ArgumentException("Root does not match store");
        MemoryProbe.FullGc();
        long readyPrivate = MemoryProbe.CurrentPrivateBytes();
        var engine = new SearchEngine(snapshot);

        var cold = new List<object>();
        foreach (var (group, request) in ColdRequests(namesOnly))
        {
            var watch = Stopwatch.StartNew();
            var page = engine.Search(request); watch.Stop();
            cold.Add(new { group, request, elapsedMs = watch.Elapsed.TotalMilliseconds, returned = page.Rows.Count, warnings = page.Warnings.Count });
        }

        var measures = new List<object>();
        long searchPeak;
        await using (var searchMemory = new MemoryProbe())
        {
            var requests = Requests(namesOnly);
            var random = new Random(20260905);
            for (int i = requests.Count - 1; i > 0; i--) { int j = random.Next(i + 1); (requests[i], requests[j]) = (requests[j], requests[i]); }
            foreach (var (group, request) in requests)
            {
                var times = new List<double>(repetitions); int errors = 0, returned = 0;
                for (int i = -1; i < repetitions; i++)
                {
                    var watch = Stopwatch.StartNew();
                    try { var page = engine.Search(request); returned = page.Rows.Count; if (page.Warnings.Count != 0) errors++; }
                    catch (Exception ex) { errors++; Console.WriteLine(ex.Message); }
                    watch.Stop(); if (i >= 0) times.Add(watch.Elapsed.TotalMilliseconds);
                }
                times.Sort();
                double P(double p) => times[Math.Max(0, (int)Math.Ceiling(p * times.Count) - 1)];
                var measure = new { group, request, repetitions, returned, errors, p50 = P(.50), p95 = P(.95), p99 = P(.99), max = times[^1], samplesMs = times };
                measures.Add(measure); Console.WriteLine($"{group} {request.FileQuery}/{request.ContentQuery} cs={request.CaseSensitive} p95={P(.95):F2}ms");
                WritePartial();
            }
            searchPeak = searchMemory.PeakPrivateBytes;
        }

        var final = new
        {
            phase = namesOnly ? "names-search" : "content-search", complete = true, utc = DateTime.UtcNow,
            machine = Machine(), files = snapshot.Files.Length, loadMs, beforeLoadPrivateBytes = beforeLoad,
            loadPeakPrivateBytes = loadPeak, readyPrivateBytes = readyPrivate, searchPeakPrivateBytes = searchPeak,
            cold, measures, binarySha256 = BinaryHash()
        };
        File.WriteAllText(result, JsonSerializer.Serialize(final, Json));
        return;

        void WritePartial() => File.WriteAllText(result, JsonSerializer.Serialize(new
        {
            phase = namesOnly ? "names-search" : "content-search", complete = false, utc = DateTime.UtcNow,
            files = snapshot.Files.Length, loadMs, readyPrivateBytes = readyPrivate, cold, measures
        }, Json));
    }

    private static List<(string Group, SearchRequest Request)> Requests(bool namesOnly)
    {
        var requests = new List<(string, SearchRequest)>();
        foreach (bool cs in new[] { false, true })
        {
            foreach (string q in new[] { "", "main", ".txt", "日本語", "*.json", "file_000000?", "source log", "absent_ASTRA_9f23c751" })
                requests.Add(("filename", new SearchRequest(q, CaseSensitive: cs)));
            foreach (string q in new[] { "source", "日本語", "*.txt", "source file", "absent_ASTRA_9f23c751" })
                requests.Add(("path", new SearchRequest(q, Scope: FileScope.FullPath, CaseSensitive: cs)));
            if (namesOnly) continue;
            foreach (string q in new[] { "PersonalRag", "configuration", "request_id", "日本語", "ERROR", "absent_ASTRA_9f23c751" })
                requests.Add(("normal", new SearchRequest(ContentQuery: q, CaseSensitive: cs)));
            foreach (string q in new[] { "request_[0-9]+", "^ERROR.*timeout" })
                requests.Add(("normal", new SearchRequest(ContentQuery: q, Mode: ContentMode.Regex, CaseSensitive: cs)));
            foreach (string q in new[] { "config*value", "存在しない*終端" })
                requests.Add(("normal", new SearchRequest(ContentQuery: q, Mode: ContentMode.Wildcard, CaseSensitive: cs)));
            foreach (string q in new[] { "th", "in", "er", "00", "日本" })
                requests.Add(("short", new SearchRequest(ContentQuery: q, CaseSensitive: cs)));
        }
        return requests;
    }

    private static IEnumerable<(string Group, SearchRequest Request)> ColdRequests(bool namesOnly)
    {
        yield return ("filename-cold", new SearchRequest("absent_ASTRA_9f23c751"));
        if (!namesOnly)
        {
            yield return ("content-cold-normal", new SearchRequest(ContentQuery: "configuration"));
            yield return ("content-cold-short", new SearchRequest(ContentQuery: "th"));
        }
    }

    private static object Machine() => new
    {
        os = RuntimeInformation.OSDescription, framework = RuntimeInformation.FrameworkDescription,
        processor = Environment.GetEnvironmentVariable("PROCESSOR_IDENTIFIER"), logicalProcessors = Environment.ProcessorCount,
        workingSetBytes = Environment.WorkingSet
    };

    private static string BinaryHash() => Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(typeof(SearchEngine).Assembly.Location)));
}

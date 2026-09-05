using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Astra.Core;

System.Globalization.CultureInfo.DefaultThreadCurrentCulture = System.Globalization.CultureInfo.InvariantCulture;
var json = new JsonSerializerOptions { WriteIndented = true };
void WriteJson(string path, object value) => File.WriteAllText(path, JsonSerializer.Serialize(value, json));
if (args.Length < 2) throw new ArgumentException("generate ROOT GiB | names ROOT COUNT | run ROOT STORE RESULT [REPETITIONS] | churn ROOT STORE RESULT COUNT");
switch (args[0])
{
    case "generate":
    case "names":
    {
        string root = Path.GetFullPath(args[1]);
        if (Directory.Exists(root)) throw new IOException("Generator requires a new root; existing corpus is never overwritten");
        Directory.CreateDirectory(root);
        bool names = args[0] == "names";
        long bytes = names ? long.Parse(args[2]) * 8192 : checked((long)(double.Parse(args[2], System.Globalization.CultureInfo.InvariantCulture) * 1073741824));
        int fileSize = names ? 8192 : 1048576;
        int count = checked((int)((bytes + fileSize - 1) / fileSize));
        var watch = Stopwatch.StartNew(); int completed = 0;
        Parallel.For(0, count, new ParallelOptions { MaxDegreeOfParallelism = 8 }, i =>
        {
            int kind = i % 5;
            string directory = Path.Combine(root, kind switch { 0 => "source", 1 => "log", 2 => "json", 3 => "csv", _ => "日本語" }, (i / 1000).ToString("D5"));
            Directory.CreateDirectory(directory);
            string extension = kind switch { 0 => ".cs", 1 => ".log", 2 => ".json", 3 => ".csv", _ => ".txt" };
            string path = Path.Combine(directory, $"file_{i:D9}{extension}");
            var random = new Random(unchecked(20260905 + i * 7919));
            int target = (int)Math.Min(fileSize, bytes - (long)i * fileSize);
            Encoding encoding = (i % 20) switch { 18 => new UnicodeEncoding(false, true), 19 => new UnicodeEncoding(true, true), _ => new UTF8Encoding(false) };
            using var output = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None, 65536);
            var preamble = encoding.GetPreamble(); output.Write(preamble);
            int line = 0;
            while (output.Position < target)
            {
                int id = random.Next(1000000000); int choice = random.Next(12); string word = Words[choice];
                string text = kind switch
                {
                    0 => $"public static string request_{id}(int value) {{ return configuration + \"{word}_{random.Next():x8}\"; }} // the interface\n",
                    1 => $"{(line % 31 == 0 ? "ERROR" : "INFO")} 2026-09-05T{line % 24:D2}:00:{line % 60:D2} request_id={id} event={word} elapsed={random.Next(500)}ms {(line % 31 == 0 ? "timeout" : "finished")}\n",
                    2 => $"{{\"request_id\":{id},\"type\":\"{word}\",\"configuration\":{{\"value\":{random.Next()},\"enabled\":true}},\"message\":\"PersonalRag in service\"}}\n",
                    3 => $"{id},{word},{random.NextDouble():F8},00,th,in,er,configuration=value\n",
                    _ => $"日本語の検索記録 {id}。設定 {word} を確認します。利用者{random.Next(100000)}の要求を処理しました。PersonalRag 文書番号{line}。\n"
                };
                var data = encoding.GetBytes(text); long remaining = target - output.Position;
                if (data.Length <= remaining) output.Write(data);
                else
                {
                    // Exact size without cutting a multibyte character or creating invalid text.
                    byte[] pad = encoding.GetBytes(new string(' ', (int)(remaining / (encoding is UnicodeEncoding ? 2 : 1))));
                    output.Write(pad);
                    if (output.Position != target) throw new InvalidOperationException("Unaligned UTF16 target size");
                }
                line++;
            }
            int done = Interlocked.Increment(ref completed); if (done % 1000 == 0) Console.WriteLine($"generated {done}/{count} ({watch.Elapsed.TotalSeconds:F1}s)");
        });
        // Manifest is outside the source corpus and is not counted as source bytes.
        WriteJson(root + ".manifest.json", new { generator = "astra-corpus-v1", seed = 20260905, names, count, bytes, fileSize,
            utc = DateTime.UtcNow, seconds = watch.Elapsed.TotalSeconds, framework = RuntimeInformation.FrameworkDescription });
        Console.WriteLine($"Generated {count} files / {bytes} bytes in {watch.Elapsed.TotalSeconds:F2}s"); break;
    }
    case "run":
    {
        string root = Path.GetFullPath(args[1]), store = Path.GetFullPath(args[2]), result = Path.GetFullPath(args[3]);
        int repetitions = args.Length > 4 ? int.Parse(args[4]) : 100;
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        long peak = 0; using var sampleStop = new CancellationTokenSource();
        var sampler = Task.Run(async () => { while (!sampleStop.IsCancellationRequested) { peak = Math.Max(peak, Process.GetCurrentProcess().PrivateMemorySize64); await Task.Delay(20); } });
        var build = Stopwatch.StartNew(); var snapshot = new IndexBuilder().Build(root, progress: n => { if (n % 10000 == 0) Console.WriteLine($"indexed {n}"); });
        IndexStore.Save(store, snapshot); build.Stop();
        long buildPeak = peak; var restart = Stopwatch.StartNew(); snapshot = IndexStore.Load(store); restart.Stop();
        var engine = new SearchEngine(snapshot);
        long sourceBytes = snapshot.Files.Sum(f => f.Size), persistentBytes = IndexStore.PersistentBytes(store);
        var baseline = new { utc = DateTime.UtcNow, os = RuntimeInformation.OSDescription, framework = RuntimeInformation.FrameworkDescription,
            processor = Environment.GetEnvironmentVariable("PROCESSOR_IDENTIFIER"), logicalProcessors = Environment.ProcessorCount,
            files = snapshot.Files.Length, sourceBytes, persistentBytes, ratio = (double)persistentBytes / sourceBytes,
            buildSeconds = build.Elapsed.TotalSeconds, restartSeconds = restart.Elapsed.TotalSeconds, buildPeakPrivateBytes = buildPeak,
            unsearchable = snapshot.Files.Count(f => f.Unsearchable != null), binarySha256 = Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(typeof(SearchEngine).Assembly.Location))) };
        WriteJson(result + ".build.json", baseline);
        Console.WriteLine(JsonSerializer.Serialize(baseline, json));
        var measures = new List<object>();
        var requests = new List<(string Group, SearchRequest Request)>();
        foreach (bool cs in new[] { false, true })
        {
            foreach (string q in new[] { "", "main", ".txt", "日本語", "*.json", "file_000000?", "source log", "absent_ASTRA_9f23c751" }) requests.Add(("filename", new SearchRequest(q, CaseSensitive: cs)));
            foreach (string q in new[] { "source", "日本語", "*.txt", "source file", "absent_ASTRA_9f23c751" }) requests.Add(("path", new SearchRequest(q, Scope: FileScope.FullPath, CaseSensitive: cs)));
            foreach (string q in new[] { "PersonalRag", "configuration", "request_id", "日本語", "ERROR", "absent_ASTRA_9f23c751" }) requests.Add(("normal", new SearchRequest(ContentQuery: q, CaseSensitive: cs)));
            foreach (string q in new[] { "request_[0-9]+", "^ERROR.*timeout" }) requests.Add(("normal", new SearchRequest(ContentQuery: q, Mode: ContentMode.Regex, CaseSensitive: cs)));
            foreach (string q in new[] { "config*value", "存在しない*終端" }) requests.Add(("normal", new SearchRequest(ContentQuery: q, Mode: ContentMode.Wildcard, CaseSensitive: cs)));
            foreach (string q in new[] { "th", "in", "er", "00", "日本" }) requests.Add(("short", new SearchRequest(ContentQuery: q, CaseSensitive: cs)));
        }
        var random = new Random(20260905); requests = requests.OrderBy(_ => random.Next()).ToList();
        foreach (var (group, request) in requests)
        {
            var times = new List<double>(); int errors = 0, returned = 0;
            for (int i = -1; i < repetitions; i++)
            {
                var time = Stopwatch.StartNew();
                try { var page = engine.Search(request); returned = page.Rows.Count; if (page.Warnings.Count != 0) errors++; }
                catch (Exception ex) { errors++; Console.WriteLine(ex.Message); }
                if (i >= 0) times.Add(time.Elapsed.TotalMilliseconds);
            }
            times.Sort();
            double P(double p) => times[Math.Max(0, (int)Math.Ceiling(p * times.Count) - 1)];
            var measure = new { group, request, repetitions, returned, errors, p50 = P(.50), p95 = P(.95), p99 = P(.99), max = times[^1], samplesMs = times };
            measures.Add(measure); Console.WriteLine($"{group} {request.FileQuery}/{request.ContentQuery} cs={request.CaseSensitive} p95={P(.95):F2}ms");
            WriteJson(result, new { baseline, measures, complete = false });
        }
        sampleStop.Cancel(); await sampler;
        WriteJson(result, new { baseline, measures, complete = true, peakPrivateBytes = peak });
        break;
    }
    default: throw new ArgumentException("Unknown benchmark command");
}

partial class Program
{
    private static readonly string[] Words = ["configuration", "initialize", "transaction", "document", "worker", "storage", "network", "database", "PersonalRag", "request", "response", "validation"];
}

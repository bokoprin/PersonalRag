using System.Diagnostics;
using System.Text;
using System.Text.Json;
using Astra.Core;

internal static class UpdateLatencyBench
{
    public static async Task Run(string root, string store, string result, int samples)
    {
        if (samples < 20) throw new ArgumentOutOfRangeException(nameof(samples), "At least 20 samples are required for p95");
        root = Path.GetFullPath(root); store = Path.GetFullPath(store); result = Path.GetFullPath(result);
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        var values = new Dictionary<string, List<double>>(StringComparer.Ordinal)
        {
            ["create"] = [], ["modify"] = [], ["rename"] = [], ["move"] = [], ["delete"] = []
        };
        string owned = Path.Combine(root, "astra-update-latency-" + Guid.NewGuid().ToString("N"));
        string movedDir = Path.Combine(owned, "moved"); Directory.CreateDirectory(movedDir);
        await using var runtime = new IndexRuntime(store, IndexStore.Load(store));
        try
        {
            await ChurnBench.Until(() => runtime.IsSettled, TimeSpan.FromMinutes(5), "initial settle");
            for (int i = 0; i < samples; i++)
            {
                string original = Path.Combine(owned, $"sample_{i:D5}.txt");
                string renamed = Path.Combine(owned, $"renamed_{i:D5}.txt");
                string moved = Path.Combine(movedDir, $"renamed_{i:D5}.txt");
                string first = $"create_token_{i:D5}\n" + new string('x', 4096);
                string second = $"modify_token_{i:D5}\n" + new string('y', 8192);

                values["create"].Add(await Measure(async () =>
                {
                    File.WriteAllText(original, first, new UTF8Encoding(false));
                    await Until(() => runtime.Snapshot.Files.Any(f => f.Path.Equals(original, StringComparison.OrdinalIgnoreCase)));
                }));

                values["modify"].Add(await Measure(async () =>
                {
                    File.WriteAllText(original, second, new UTF8Encoding(false));
                    await Until(() => new SearchEngine(runtime.Snapshot).Search(new SearchRequest(ContentQuery: $"modify_token_{i:D5}")).Rows.Count == 1);
                }));

                values["rename"].Add(await Measure(async () =>
                {
                    File.Move(original, renamed);
                    await Until(() => runtime.Snapshot.Files.Any(f => f.Path.Equals(renamed, StringComparison.OrdinalIgnoreCase)) &&
                                      !runtime.Snapshot.Files.Any(f => f.Path.Equals(original, StringComparison.OrdinalIgnoreCase)));
                }));

                values["move"].Add(await Measure(async () =>
                {
                    File.Move(renamed, moved);
                    await Until(() => runtime.Snapshot.Files.Any(f => f.Path.Equals(moved, StringComparison.OrdinalIgnoreCase)) &&
                                      !runtime.Snapshot.Files.Any(f => f.Path.Equals(renamed, StringComparison.OrdinalIgnoreCase)));
                }));

                values["delete"].Add(await Measure(async () =>
                {
                    File.Delete(moved);
                    await Until(() => !runtime.Snapshot.Files.Any(f => f.Path.Equals(moved, StringComparison.OrdinalIgnoreCase)));
                }));
            }
            Directory.Delete(movedDir); Directory.Delete(owned);
            await ChurnBench.Until(() => runtime.IsSettled, TimeSpan.FromMinutes(5), "final settle");

            var phases = values.ToDictionary(kv => kv.Key, kv => Stats(kv.Value));
            bool pass = phases["create"].P95 <= 1000 && phases["rename"].P95 <= 1000 && phases["move"].P95 <= 1000 &&
                        phases["delete"].P95 <= 1000 && phases["modify"].P95 <= 2000;
            File.WriteAllText(result, JsonSerializer.Serialize(new { complete = true, pass, samples, phases, utc = DateTime.UtcNow },
                new JsonSerializerOptions { WriteIndented = true }));
            Console.WriteLine($"UPDATE LATENCY {(pass ? "PASS" : "FAIL")} create={phases["create"].P95:F1}ms modify={phases["modify"].P95:F1}ms rename={phases["rename"].P95:F1}ms move={phases["move"].P95:F1}ms delete={phases["delete"].P95:F1}ms");
        }
        finally
        {
            try { if (Directory.Exists(owned)) Directory.Delete(owned, true); } catch { }
        }

        async Task Until(Func<bool> condition)
        {
            var watch = Stopwatch.StartNew();
            while (!condition()) { if (watch.Elapsed > TimeSpan.FromSeconds(10)) throw new TimeoutException("update visibility"); await Task.Delay(5); }
        }
        static async Task<double> Measure(Func<Task> action)
        { var watch = Stopwatch.StartNew(); await action(); watch.Stop(); return watch.Elapsed.TotalMilliseconds; }
    }

    private static PhaseStats Stats(List<double> values)
    {
        values.Sort();
        double P(double p) => values[Math.Max(0, (int)Math.Ceiling(p * values.Count) - 1)];
        return new PhaseStats(P(.50), P(.95), P(.99), values[^1]);
    }
    internal sealed record PhaseStats(double P50, double P95, double P99, double Max);
}

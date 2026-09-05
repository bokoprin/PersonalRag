using System.Diagnostics;
using System.Text;
using System.Text.Json;
using Astra.Core;

internal static class ChurnBench
{
    public static async Task Run(string root, string store, string result, int count)
    {
        root = Path.GetFullPath(root); store = Path.GetFullPath(store); result = Path.GetFullPath(result);
        var original = IndexStore.Load(store);
        if (!StringComparer.OrdinalIgnoreCase.Equals(root, original.Root)) throw new ArgumentException("Root does not match store");
        string owned = Path.Combine(root, "astra-churn-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(owned);
        var phases = new List<object>();
        long peak = 0; using var monitorStop = new CancellationTokenSource();
        var monitor = Task.Run(async () => { while (!monitorStop.IsCancellationRequested) { peak = Math.Max(peak, Process.GetCurrentProcess().PrivateMemorySize64); await Task.Delay(25); } });
        try
        {
            await using (var runtime = new IndexRuntime(store, original))
            {
                await Until(() => runtime.Status == "Ready", TimeSpan.FromMinutes(5), "initial catchup");
                var watch = Stopwatch.StartNew();
                string FileAt(int i, bool renamed = false) => Path.Combine(owned, $"{(renamed ? "renamed" : "created")}_{i:D5}.txt");
                string TextAt(int i, bool modified = false) => $"{(modified ? "modified" : "created")}_token_{i:D5}\n" + new string(modified ? 'y' : 'x', modified ? 32768 : 32700);
                Parallel.For(0, count, new ParallelOptions { MaxDegreeOfParallelism = 4 }, i => File.WriteAllText(FileAt(i), TextAt(i), new UTF8Encoding(false)));
                await Until(() => runtime.Snapshot.Files.Length == original.Files.Length + count, TimeSpan.FromMinutes(5), "create");
                VerifyRows(runtime.Snapshot, "created_token_", count);
                phases.Add(new { phase = "create", count, seconds = watch.Elapsed.TotalSeconds }); Save();
                watch.Restart();
                Parallel.For(0, count, new ParallelOptions { MaxDegreeOfParallelism = 4 }, i => File.WriteAllText(FileAt(i), TextAt(i, true), new UTF8Encoding(false)));
                long modifiedSize = Encoding.UTF8.GetByteCount(TextAt(0, true));
                await Until(() => runtime.Snapshot.Files.Count(f => f.Path.StartsWith(owned, StringComparison.OrdinalIgnoreCase) && f.Size == modifiedSize) == count,
                    TimeSpan.FromMinutes(5), "modify");
                VerifyRows(runtime.Snapshot, "modified_token_", count);
                VerifyRows(runtime.Snapshot, "created_token_", 0);
                phases.Add(new { phase = "modify", count, seconds = watch.Elapsed.TotalSeconds }); Save();
                watch.Restart();
                for (int i = 0; i < count; i++) File.Move(FileAt(i), FileAt(i, true));
                await Until(() => runtime.Snapshot.Files.Count(f => f.Path.StartsWith(owned, StringComparison.OrdinalIgnoreCase) && f.Name.StartsWith("renamed_")) == count,
                    TimeSpan.FromMinutes(5), "rename");
                VerifyRows(runtime.Snapshot, "modified_token_", count);
                phases.Add(new { phase = "rename", count, seconds = watch.Elapsed.TotalSeconds }); Save();
                watch.Restart();
                for (int i = 0; i < count; i++) File.Delete(FileAt(i, true));
                Directory.Delete(owned); // Only the empty directory created by this invocation.
                await Until(() => runtime.Snapshot.Files.Length == original.Files.Length && !runtime.Snapshot.Files.Any(f => f.Path.StartsWith(owned, StringComparison.OrdinalIgnoreCase)),
                    TimeSpan.FromMinutes(5), "delete");
                VerifyRows(runtime.Snapshot, "modified_token_", 0);
                phases.Add(new { phase = "delete", count, seconds = watch.Elapsed.TotalSeconds }); Save();
            }
            var restarted = IndexStore.Load(store);
            var actual = Directory.EnumerateFiles(root, "*", new EnumerationOptions { RecurseSubdirectories = true, AttributesToSkip = FileAttributes.ReparsePoint, IgnoreInaccessible = false })
                .Order(StringComparer.OrdinalIgnoreCase).ToArray();
            if (!actual.SequenceEqual(restarted.Files.Select(f => f.Path), StringComparer.OrdinalIgnoreCase)) throw new Exception("Filesystem/restart mismatch");
            if (!original.Files.Select(f => (f.Path, f.Size, f.ModifiedUtcTicks)).SequenceEqual(restarted.Files.Select(f => (f.Path, f.Size, f.ModifiedUtcTicks))))
                throw new Exception("Original corpus metadata changed");
            long bytes = IndexStore.PersistentBytes(store), source = restarted.Files.Sum(f => f.Size);
            monitorStop.Cancel(); await monitor;
            File.WriteAllText(result, JsonSerializer.Serialize(new { complete = true, pass = bytes <= source * .05 && peak <= 4L * 1073741824,
                phases, count, sourceBytes = source, persistentBytes = bytes, ratio = (double)bytes / source, peakPrivateBytes = peak,
                restartFiles = restarted.Files.Length, utc = DateTime.UtcNow }, new JsonSerializerOptions { WriteIndented = true }));
            Console.WriteLine($"CHURN complete {count} x 4 / ratio {(double)bytes / source:P4} / peak {peak}");
        }
        catch (Exception ex)
        {
            File.WriteAllText(result, JsonSerializer.Serialize(new { complete = false, pass = false, phases, error = ex.ToString(), owned })); throw;
        }
        finally { monitorStop.Cancel(); await monitor; }
        void Save()
        {
            Console.WriteLine(JsonSerializer.Serialize(phases[^1]));
            File.WriteAllText(result, JsonSerializer.Serialize(new { complete = false, phases }, new JsonSerializerOptions { WriteIndented = true }));
        }
    }
    private static void VerifyRows(IndexSnapshot snapshot, string query, int expected)
    {
        var engine = new SearchEngine(snapshot); int next = 0; var found = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        while (true)
        {
            var page = engine.Search(new SearchRequest(ContentQuery: query), next);
            if (page.Warnings.Count != 0) throw new Exception("Unexpected unreadable file");
            foreach (var row in page.Rows)
            {
                if (!found.Add(row.File.Path) || !File.ReadAllText(row.File.Path).Contains(query, StringComparison.Ordinal)) throw new Exception("Oracle mismatch");
            }
            if (page.Complete) break; next = page.NextOffset;
        }
        if (found.Count != expected) throw new Exception($"Expected {expected} matches for {query}, got {found.Count}");
    }
    internal static async Task Until(Func<bool> condition, TimeSpan timeout, string phase)
    {
        var watch = Stopwatch.StartNew();
        while (!condition()) { if (watch.Elapsed > timeout) throw new TimeoutException(phase); await Task.Delay(50); }
    }
}

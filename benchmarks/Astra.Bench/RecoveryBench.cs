using System.Diagnostics;
using System.Text.Json;
using Astra.Core;

internal static class RecoveryBench
{
    public static async Task Run(string store, string result)
    {
        store = Path.GetFullPath(store);
        result = Path.GetFullPath(result);
        Directory.CreateDirectory(Path.GetDirectoryName(result)!);
        var before = IndexStore.Load(store);
        string marker = Path.Combine(Path.GetDirectoryName(store)!, "crash-write-" + Guid.NewGuid().ToString("N") + ".ready");
        string processPath = Environment.ProcessPath ?? throw new InvalidOperationException("Current process path is unavailable");
        var startInfo = new ProcessStartInfo(processPath) { UseShellExecute = false, CreateNoWindow = true };
        if (Path.GetFileNameWithoutExtension(processPath).Equals("dotnet", StringComparison.OrdinalIgnoreCase))
            startInfo.ArgumentList.Add(typeof(RecoveryBench).Assembly.Location);
        startInfo.ArgumentList.Add("crash-write");
        startInfo.ArgumentList.Add(store);
        startInfo.ArgumentList.Add(marker);
        using var process = Process.Start(startInfo) ?? throw new Exception("Cannot launch crash test child");
        try
        {
            await ChurnBench.Until(() => File.Exists(marker) || process.HasExited, TimeSpan.FromMinutes(2), "child reaches pre-commit");
            if (!File.Exists(marker)) throw new Exception($"Crash window missed; child exited with code {process.ExitCode}");
            if (process.HasExited) throw new Exception("Crash window missed; child exited before it could be killed");
            if (!Directory.EnumerateFiles(store, "snapshot-*.tmp").Any()) throw new Exception("Crash marker appeared without a temporary snapshot");
            process.Kill(true); await process.WaitForExitAsync();
            var restarted = IndexStore.Load(store);
            if (!before.Files.Select(f => (f.Path, f.Size, f.ModifiedUtcTicks)).SequenceEqual(restarted.Files.Select(f => (f.Path, f.Size, f.ModifiedUtcTicks))))
                throw new Exception("Crash changed committed snapshot");
            IndexStore.Save(store, restarted);
            if (Directory.EnumerateFiles(store, "snapshot-*.tmp").Any()) throw new Exception("Abandoned generation remains");
            File.WriteAllText(result, JsonSerializer.Serialize(new { complete = true, pass = true, killedDuringWrite = true, files = restarted.Files.Length,
                persistentBytes = IndexStore.PersistentBytes(store), utc = DateTime.UtcNow }, new JsonSerializerOptions { WriteIndented = true }));
            Console.WriteLine("RECOVERY PASS");
        }
        finally
        {
            if (!process.HasExited)
            {
                process.Kill(true);
                await process.WaitForExitAsync();
            }
            try { if (File.Exists(marker)) File.Delete(marker); } catch { }
        }
    }
}

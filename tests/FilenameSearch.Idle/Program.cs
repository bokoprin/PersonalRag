using System.Diagnostics;
using System.Text.Json;
using PersonalRag.FilenameSearch;

if (!OperatingSystem.IsWindows())
{
    Console.WriteLine("SKIP FilenameSearch idle test: Windows product acceptance requires Windows");
    return;
}

if (args.Length < 3) throw new ArgumentException("usage: FilenameSearch.Idle ROOT STORE REPORT [SECONDS]");
string root = Path.GetFullPath(args[0]);
string store = Path.GetFullPath(args[1]);
string reportPath = Path.GetFullPath(args[2]);
int seconds = args.Length >= 4 ? int.Parse(args[3]) : 600;
if (seconds <= 0) throw new ArgumentOutOfRangeException(nameof(args), "Idle duration must be positive");

var output = new Dictionary<string, object?>
{
    ["version"] = 1,
    ["root"] = root,
    ["store"] = store,
    ["requested_seconds"] = seconds,
    ["started_utc"] = DateTime.UtcNow
};
try
{
    await using var catalog = FileSystemCatalog.Open(root, store);
    await catalog.Ready;
    await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
    if (catalog.Search(new SearchRequest("", Limit: 1)).Records.Count != 1)
        throw new InvalidOperationException("Existing index is not queryable before idle sampling");
    await Task.Delay(TimeSpan.FromSeconds(3));
    var process = Process.GetCurrentProcess();
    process.Refresh();
    TimeSpan cpuBefore = process.TotalProcessorTime;
    var wall = Stopwatch.StartNew();
    FileInfo before = new(store);
    long lengthBefore = before.Exists ? before.Length : -1;
    DateTime writeBefore = before.Exists ? before.LastWriteTimeUtc : DateTime.MinValue;
    await Task.Delay(TimeSpan.FromSeconds(seconds));
    wall.Stop();
    process.Refresh();
    TimeSpan cpuAfter = process.TotalProcessorTime;
    FileInfo after = new(store);
    bool storeUnchanged = after.Exists && after.Length == lengthBefore && after.LastWriteTimeUtc == writeBefore;
    TimeSpan cpu = cpuAfter - cpuBefore;
    double cpuPercent = wall.Elapsed.TotalMilliseconds > 0
        ? cpu.TotalMilliseconds / wall.Elapsed.TotalMilliseconds / Environment.ProcessorCount * 100
        : double.PositiveInfinity;
    output["actual_seconds"] = wall.Elapsed.TotalSeconds;
    output["cpu_percent"] = cpuPercent;
    output["store_unchanged"] = storeUnchanged;
    output["private_bytes"] = process.PrivateMemorySize64;
    output["entries"] = catalog.Records.Count;
    output["pass"] = cpuPercent <= 1.0 && storeUnchanged;
}
catch (Exception ex)
{
    output["pass"] = false;
    output["error"] = ex.ToString();
    throw;
}
finally
{
    output["finished_utc"] = DateTime.UtcNow;
    Directory.CreateDirectory(Path.GetDirectoryName(reportPath)!);
    File.WriteAllText(reportPath, JsonSerializer.Serialize(output, new JsonSerializerOptions { WriteIndented = true }));
}

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
    double settleSeconds = await WaitForStoreStableAsync(store, TimeSpan.FromSeconds(30));
    output["initialization_settle_seconds"] = settleSeconds;
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

static async Task<double> WaitForStoreStableAsync(string store, TimeSpan timeout)
{
    var watch = Stopwatch.StartNew();
    FileInfo initial = new(store);
    if (!initial.Exists) throw new FileNotFoundException("Store was not created before idle sampling", store);
    long length = initial.Length;
    DateTime writeTime = initial.LastWriteTimeUtc;
    int stableSamples = 0;
    while (watch.Elapsed < timeout)
    {
        await Task.Delay(250);
        FileInfo current = new(store);
        if (current.Exists && current.Length == length && current.LastWriteTimeUtc == writeTime)
        {
            if (++stableSamples >= 4) return watch.Elapsed.TotalSeconds;
            continue;
        }
        if (!current.Exists) throw new FileNotFoundException("Store disappeared during initialization", store);
        length = current.Length;
        writeTime = current.LastWriteTimeUtc;
        stableSamples = 0;
    }
    throw new TimeoutException("Store did not settle before idle sampling");
}

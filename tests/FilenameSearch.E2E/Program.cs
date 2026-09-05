using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using PersonalRag.FilenameSearch;

if (!OperatingSystem.IsWindows())
{
    Console.WriteLine("SKIP FilenameSearch E2E: Windows filesystem acceptance requires Windows");
    return;
}

if (args.Length > 0 && args[0].Equals("--crash-child", StringComparison.OrdinalIgnoreCase))
{
    await RunCrashChild(args.Skip(1).ToArray());
    return;
}

if (args.Length < 2) throw new ArgumentException("usage: FilenameSearch.E2E <root> <report.json> [count]");
string root = Path.GetFullPath(args[0]);
string reportPath = Path.GetFullPath(args[1]);
int count = args.Length >= 3 ? int.Parse(args[2]) : 100_000;
bool keepRoot = args.Skip(3).Any(argument => argument.Equals("--keep", StringComparison.OrdinalIgnoreCase));
bool runChurn = args.Skip(3).Any(argument => argument.Equals("--churn", StringComparison.OrdinalIgnoreCase));
if (count < 100_000) throw new ArgumentOutOfRangeException(nameof(args), "E2E requires at least 100,000 entries");
string? parent = Path.GetDirectoryName(root);
if (parent is null || !Path.GetFullPath(root).StartsWith(Path.GetFullPath(Path.GetTempPath()), StringComparison.OrdinalIgnoreCase))
    throw new InvalidOperationException("E2E root must be under the system temporary directory");
Directory.CreateDirectory(parent);
Directory.CreateDirectory(root);
// Keep the store below the dedicated root so cleanup can never touch a shared temp folder.
string store = Path.Combine(root, ".personalrag-store", "index.routec");
var report = new Dictionary<string, object?> { ["version"] = 1, ["root"] = root, ["count_requested"] = count, ["started_utc"] = DateTime.UtcNow };
var stopwatch = Stopwatch.StartNew();
int checks = 0;
void Check(bool condition, string name)
{
    checks++;
    if (!condition) throw new Exception("FAIL: " + name);
}

try
{
    GenerateCorpus(root, count);
    string longPath = CreateLongPathFixture(root);
    string churnDirectory = Path.Combine(root, "churn");
    string churnMoveDirectory = Path.Combine(root, "churn-moved");
    Directory.CreateDirectory(churnDirectory);
    Directory.CreateDirectory(churnMoveDirectory);
    report["generation_seconds"] = stopwatch.Elapsed.TotalSeconds;
    report["long_path_length"] = longPath.Length;
    await using (var catalog = FileSystemCatalog.Open(root, store))
    {
        await catalog.Ready;
        await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
        report["initial_entries"] = catalog.Records.Count;
        report["ready_private_bytes"] = Process.GetCurrentProcess().PrivateMemorySize64;
        Check(catalog.Records.Count >= count + 1, "all generated files plus root are indexed");
        var firstBatchSamples = new List<double>(20);
        FilenameSearchResult firstBatch = FilenameSearchResult.Empty();
        for (int i = 0; i < 20; i++)
        {
            var first = Stopwatch.StartNew();
            firstBatch = catalog.Search(new SearchRequest("fixture_", Limit: 100));
            first.Stop();
            firstBatchSamples.Add(first.Elapsed.TotalMilliseconds);
            Check(firstBatch.Records.Count == 100, "first useful batch is bounded at 100");
        }
        report["first_batch_ms"] = firstBatchSamples[0];
        report["first_batch_samples_ms"] = firstBatchSamples;
        report["first_batch_p95_ms"] = Percentile(firstBatchSamples, .95);
        report["first_batch_max_ms"] = firstBatchSamples.Max();
        report["first_batch_count"] = firstBatch.Records.Count;
        Check(Percentile(firstBatchSamples, .95) <= 100 && firstBatchSamples.Max() <= 200, "first useful batch latency gate");
        Check(catalog.Search(new SearchRequest("café")).Records.Count >= 2, "NFC/NFD filenames are equivalent");
        Check(catalog.Search(new SearchRequest("\\", SearchScope.FullPath, Limit: 1)).Records.Count == 1, "one-code-point path query remains correct");

        string longQuery = Path.GetFileNameWithoutExtension(longPath);
        Check(catalog.Search(new SearchRequest(longQuery)).Records.Count == 1, "supported long path is searchable");

        var createSamples = new List<double>(20);
        var deleteSamples = new List<double>(20);
        var renameSamples = new List<double>(20);
        var moveSamples = new List<double>(20);
        for (int i = 0; i < 20; i++)
        {
            string suffix = i.ToString("D2");
            string createQuery = "e2e-create-" + suffix;
            string createPath = Path.Combine(root, createQuery + ".md");
            createSamples.Add(await MeasureConvergence(catalog, createQuery, createPath, () => File.WriteAllText(createPath, "create"), expected: 1));
            deleteSamples.Add(await MeasureConvergence(catalog, createQuery, createPath, () => File.Delete(createPath), expected: 0));

            string renameQuery = "e2e-rename-target-" + suffix;
            string renameSource = Path.Combine(root, "e2e-rename-source-" + suffix + ".md");
            string renameTarget = Path.Combine(root, renameQuery + ".md");
            File.WriteAllText(renameSource, "rename");
            await Until(() => catalog.Search(new SearchRequest(Path.GetFileNameWithoutExtension(renameSource))).Records.Count == 1, "rename source create");
            renameSamples.Add(await MeasureConvergence(catalog, renameQuery, renameTarget, () => File.Move(renameSource, renameTarget), expected: 1));
            File.Delete(renameTarget);
            await Until(() => catalog.Search(new SearchRequest(renameQuery)).Records.Count == 0, "rename cleanup");

            string moveQuery = "e2e-move-target-" + suffix;
            string moveSource = Path.Combine(root, "e2e-move-source-" + suffix + ".md");
            string moveTarget = Path.Combine(root, "fixture_dir_000", moveQuery + ".md");
            File.WriteAllText(moveSource, "move");
            await Until(() => catalog.Search(new SearchRequest(Path.GetFileNameWithoutExtension(moveSource))).Records.Count == 1, "move source create");
            moveSamples.Add(await MeasureConvergence(catalog, moveQuery, moveTarget, () => File.Move(moveSource, moveTarget), expected: 1));
            Check(catalog.Search(new SearchRequest(moveQuery)).Records.Single().FullPath == moveTarget, "move path is current");
            File.Delete(moveTarget);
            await Until(() => catalog.Search(new SearchRequest(moveQuery)).Records.Count == 0, "move cleanup");
        }
        RecordLatency(report, "create", createSamples);
        RecordLatency(report, "delete", deleteSamples);
        RecordLatency(report, "rename", renameSamples);
        RecordLatency(report, "move", moveSamples);
        Check(Percentile(createSamples, .95) <= 1000 && Percentile(deleteSamples, .95) <= 1000 && Percentile(renameSamples, .95) <= 1000 && Percentile(moveSamples, .95) <= 1000, "live update latency gates");

        if (runChurn)
        {
            var churn = await RunChurnAsync(catalog, root, churnDirectory, churnMoveDirectory);
            report["churn"] = churn;
            Check((bool)churn["pass"]!, "10k churn correctness");
        }

        await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
        report["idle_private_bytes"] = Process.GetCurrentProcess().PrivateMemorySize64;
    }

    // Restart catch-up: mutate the dedicated tree while the catalog is stopped.
    string stoppedCreate = Path.Combine(root, "restart-created.md");
    File.WriteAllText(stoppedCreate, "restart");
    var restartWatch = Stopwatch.StartNew();
    await using (var restarted = FileSystemCatalog.Open(root, store))
    {
        await restarted.Ready;
        await Until(() => restarted.Search(new SearchRequest("restart-created")).Records.Count == 1, "restart catch-up");
        restartWatch.Stop();
        report["restart_catchup_ms"] = restartWatch.Elapsed.TotalMilliseconds;
        report["restart_catchup"] = true;
        Check(restartWatch.Elapsed.TotalMilliseconds <= 5000, "restart catch-up latency gate");
    }

    var crash = await RunForcedTerminationAsync(root, store);
    foreach ((string key, object? value) in crash) report[key] = value;
    Check((bool)crash["forced_termination_recovery"]!, "forced termination recovery");

    // Fail-safe recovery: the next open must rebuild rather than serve a corrupt index.
    File.WriteAllBytes(store, [0, 1, 2, 3, 4, 5]);
    await using (var recovered = FileSystemCatalog.Open(root, store))
    {
        await recovered.Ready;
        await Until(() => recovered.Search(new SearchRequest("restart-created")).Records.Count == 1, "corrupt store recovery");
        report["corrupt_store_recovery"] = true;
    }
    report["persistent_bytes"] = File.Exists(store) ? new FileInfo(store).Length : 0L;
    report["source_logical_bytes"] = LogicalSourceBytes(root, store);
    report["persistent_ratio"] = Convert.ToDouble(report["source_logical_bytes"]) == 0 ? null : (double)(long)report["persistent_bytes"]! / Convert.ToDouble(report["source_logical_bytes"]);
    report["reparse_point"] = await TryReparsePointAsync(root, store);
    report["inaccessible_directory"] = "NOT_RUN: ACL mutation requires a separately provisioned account";
    report["checks"] = checks;
    report["pass"] = true;
}
catch (Exception ex)
{
    report["checks"] = checks;
    report["pass"] = false;
    report["error"] = ex.ToString();
    throw;
}
finally
{
    report["finished_utc"] = DateTime.UtcNow;
    Directory.CreateDirectory(Path.GetDirectoryName(reportPath)!);
    File.WriteAllText(reportPath, JsonSerializer.Serialize(report, new JsonSerializerOptions { WriteIndented = true }));
    try { if (!keepRoot && Directory.Exists(root)) Directory.Delete(root, true); }
    catch (IOException) { }
}

static void GenerateCorpus(string root, int count)
{
    var random = RandomNumberGenerator.GetBytes(32);
    for (int i = 0; i < count; i++)
    {
        string directory = Path.Combine(root, $"fixture_dir_{i % 1000:D3}", i % 2 == 0 ? "設計" : "source", $"deep_{i % 16:D2}");
        Directory.CreateDirectory(directory);
        string stem = i % 40_000 == 0 ? $"café_{i:D06}" : i % 40_000 == 1 ? $"cafe\u0301-nfd_{i:D06}" : $"fixture_{i:D06}_{(i % 2 == 0 ? "ReadMe" : "report")}";
        string extension = i % 7 == 0 ? ".LOG" : ".txt";
        string path = Path.Combine(directory, stem + extension);
        File.WriteAllText(path, Convert.ToHexString(random.AsSpan(0, 4)), new UTF8Encoding(false));
    }
}

static string CreateLongPathFixture(string root)
{
    string directory = root;
    for (int i = 0; i < 3; i++) directory = Path.Combine(directory, "long-segment-" + new string('x', 18));
    Directory.CreateDirectory(directory);
    string path = Path.Combine(directory, "long_fixture_file.txt");
    File.WriteAllText(path, "long path");
    if (path.Length > 240) throw new InvalidOperationException($"Long-path fixture exceeded supported limit: {path.Length}");
    return path;
}

static void RecordLatency(Dictionary<string, object?> report, string name, IReadOnlyList<double> samples)
{
    report[name + "_samples_ms"] = samples;
    report[name + "_p50_ms"] = Percentile(samples, .50);
    report[name + "_p95_ms"] = Percentile(samples, .95);
    report[name + "_p99_ms"] = Percentile(samples, .99);
    report[name + "_max_ms"] = samples.Count == 0 ? 0 : samples.Max();
}

static double Percentile(IReadOnlyList<double> values, double percentile)
{
    if (values.Count == 0) return 0;
    double[] sorted = values.OrderBy(value => value).ToArray();
    int index = Math.Clamp((int)Math.Ceiling(sorted.Length * percentile) - 1, 0, sorted.Length - 1);
    return sorted[index];
}

static async Task<double> MeasureConvergence(FileSystemCatalog catalog, string query, string path, Action mutate, int expected)
{
    var watch = Stopwatch.StartNew();
    mutate();
    await Until(() => catalog.Search(new SearchRequest(query, Limit: 100_000)).Records.Count == expected, query + " convergence");
    watch.Stop();
    return watch.Elapsed.TotalMilliseconds;
}

static long LogicalSourceBytes(string root, string store)
{
    long total = 0;
    foreach (string path in Directory.EnumerateFiles(root, "*", new EnumerationOptions
    {
        IgnoreInaccessible = true,
        RecurseSubdirectories = true,
        AttributesToSkip = FileAttributes.ReparsePoint
    }))
    {
        if (path.StartsWith(store, StringComparison.OrdinalIgnoreCase)) continue;
        try { total = checked(total + new FileInfo(path).Length); }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
    }
    return total;
}

static async Task<Dictionary<string, object?>> RunChurnAsync(FileSystemCatalog catalog, string root, string churnDirectory, string moveDirectory)
{
    const int count = 10_000;
    string[] original = Enumerable.Range(0, count).Select(i => Path.Combine(churnDirectory, $"churnfile_{i:D05}.txt")).ToArray();
    string[] renamed = Enumerable.Range(0, count).Select(i => Path.Combine(churnDirectory, $"churnfile_{i:D05}.renamed.txt")).ToArray();
    string[] moved = Enumerable.Range(0, count).Select(i => Path.Combine(moveDirectory, $"churnfile_{i:D05}.renamed.txt")).ToArray();
    for (int i = 0; i < count; i++) File.WriteAllText(original[i], "churn");
    await WaitForEventsAsync(catalog, "churn create");
    int created = catalog.Search(new SearchRequest("churnfile_", Limit: 100_000)).Records.Count;
    for (int i = 0; i < count; i++) File.AppendAllText(original[i], "-modified");
    await WaitForEventsAsync(catalog, "churn modify");
    for (int i = 0; i < count; i++) File.Move(original[i], renamed[i]);
    await WaitForEventsAsync(catalog, "churn rename");
    for (int i = 0; i < count; i++) File.Move(renamed[i], moved[i]);
    await WaitForEventsAsync(catalog, "churn move");
    for (int i = 0; i < count; i++) File.Delete(moved[i]);
    await WaitForEventsAsync(catalog, "churn delete");
    int remaining = catalog.Search(new SearchRequest("churnfile_", Limit: 100_000)).Records.Count;
    return new Dictionary<string, object?>
    {
        ["create"] = count,
        ["modify"] = count,
        ["rename"] = count,
        ["move"] = count,
        ["delete"] = count,
        ["created_matches"] = created,
        ["remaining_matches"] = remaining,
        ["pass"] = created == count && remaining == 0
    };
}

static async Task WaitForEventsAsync(FileSystemCatalog catalog, string name)
{
    await Task.Delay(250);
    await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(5));
    if (catalog.Status != "Ready") throw new InvalidOperationException($"{name} did not become Ready: {catalog.Status}");
}

static async Task<Dictionary<string, object?>> RunForcedTerminationAsync(string root, string store)
{
    string marker = Path.Combine(root, ".crash-child-" + Guid.NewGuid().ToString("N") + ".marker");
    string crashPath = Path.Combine(root, "forced-termination-recovered.md");
    string assembly = System.Reflection.Assembly.GetExecutingAssembly().Location;
    var start = new ProcessStartInfo("dotnet") { UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true, CreateNoWindow = true };
    start.ArgumentList.Add(assembly);
    start.ArgumentList.Add("--crash-child");
    start.ArgumentList.Add(root);
    start.ArgumentList.Add(store);
    start.ArgumentList.Add(marker);
    start.ArgumentList.Add(crashPath);
    using Process child = Process.Start(start) ?? throw new InvalidOperationException("Could not start forced-termination child");
    var watch = Stopwatch.StartNew();
    await Until(() => File.Exists(marker), "crash child changed", TimeSpan.FromSeconds(30));
    try { if (!child.HasExited) child.Kill(entireProcessTree: true); }
    catch (InvalidOperationException) { }
    await child.WaitForExitAsync();
    watch.Stop();
    bool recovered = false;
    await using (var catalog = FileSystemCatalog.Open(root, store))
    {
        await catalog.Ready;
        await Until(() => catalog.Search(new SearchRequest("forced-termination-recovered")).Records.Count == 1, "forced termination recovery", TimeSpan.FromSeconds(30));
        recovered = true;
    }
    TryDeleteFile(marker);
    TryDeleteFile(crashPath);
    return new Dictionary<string, object?>
    {
        ["forced_termination_exit_code"] = child.ExitCode,
        ["forced_termination_recovery"] = recovered,
        ["forced_termination_recovery_ms"] = watch.Elapsed.TotalMilliseconds
    };
}

static async Task RunCrashChild(string[] args)
{
    if (args.Length < 4) throw new ArgumentException("usage: --crash-child ROOT STORE MARKER CRASH_PATH");
    string root = Path.GetFullPath(args[0]), store = Path.GetFullPath(args[1]), marker = Path.GetFullPath(args[2]), crashPath = Path.GetFullPath(args[3]);
    await using var catalog = FileSystemCatalog.Open(root, store);
    await catalog.Ready;
    await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
    File.WriteAllText(crashPath, "forced termination");
    await Until(() => catalog.Search(new SearchRequest("forced-termination-recovered")).Records.Count == 1, "crash child update", TimeSpan.FromSeconds(30));
    File.WriteAllText(marker, "changed");
    await Task.Delay(Timeout.InfiniteTimeSpan);
}

static async Task<Dictionary<string, object?>> TryReparsePointAsync(string root, string store)
{
    string link = Path.Combine(root, "reparse-link");
    try
    {
        Directory.CreateSymbolicLink(link, root);
        await using var catalog = FileSystemCatalog.Open(root, store);
        await catalog.Ready;
        await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
        bool indexed = catalog.Records.Any(record => record.FullPath.Equals(link, StringComparison.OrdinalIgnoreCase));
        return new Dictionary<string, object?> { ["status"] = indexed ? "FAIL" : "PASS", ["indexed"] = indexed };
    }
    catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or PlatformNotSupportedException)
    {
        return new Dictionary<string, object?> { ["status"] = "SKIPPED", ["reason"] = ex.GetType().Name };
    }
    finally { TryDeleteDirectory(link); }
}

static void TryDeleteFile(string path)
{
    try { if (File.Exists(path)) File.Delete(path); } catch (IOException) { }
    catch (UnauthorizedAccessException) { }
}

static void TryDeleteDirectory(string path)
{
    try { if (Directory.Exists(path)) Directory.Delete(path, true); } catch (IOException) { }
    catch (UnauthorizedAccessException) { }
}

static async Task Until(Func<bool> condition, string name, TimeSpan? timeout = null)
{
    using var timeoutSource = new CancellationTokenSource(timeout ?? TimeSpan.FromSeconds(30));
    while (!condition())
    {
        timeoutSource.Token.ThrowIfCancellationRequested();
        await Task.Delay(25, timeoutSource.Token);
    }
}

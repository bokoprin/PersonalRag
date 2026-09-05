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

if (args.Length < 2) throw new ArgumentException("usage: FilenameSearch.E2E <root> <report.json> [count]");
string root = Path.GetFullPath(args[0]);
string reportPath = Path.GetFullPath(args[1]);
int count = args.Length >= 3 ? int.Parse(args[2]) : 100_000;
if (count < 100_000) throw new ArgumentOutOfRangeException(nameof(args), "E2E requires at least 100,000 entries");
string? parent = Path.GetDirectoryName(root);
if (parent is null || !Path.GetFullPath(root).StartsWith(Path.GetFullPath(Path.GetTempPath()), StringComparison.OrdinalIgnoreCase))
    throw new InvalidOperationException("E2E root must be under the system temporary directory");
Directory.CreateDirectory(parent);
string store = Path.Combine(parent, "store", "index.routec");
Directory.CreateDirectory(root);
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
    report["generation_seconds"] = stopwatch.Elapsed.TotalSeconds;
    await using (var catalog = FileSystemCatalog.Open(root, store))
    {
        await catalog.Ready;
        await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
        report["initial_entries"] = catalog.Records.Count;
        report["ready_private_bytes"] = Process.GetCurrentProcess().PrivateMemorySize64;
        Check(catalog.Records.Count >= count + 1, "all generated files plus root are indexed");
        var first = Stopwatch.StartNew();
        FilenameSearchResult firstBatch = catalog.Search(new SearchRequest("fixture_", Limit: 100));
        first.Stop();
        report["first_batch_ms"] = first.Elapsed.TotalMilliseconds;
        report["first_batch_count"] = firstBatch.Records.Count;
        Check(firstBatch.Records.Count == 100, "first useful batch is bounded at 100");
        Check(catalog.Search(new SearchRequest("café")).Records.Count >= 2, "NFC/NFD filenames are equivalent");
        Check(catalog.Search(new SearchRequest("\\", SearchScope.FullPath, Limit: 1)).Records.Count == 1, "one-code-point path query remains correct");

        report["create_ms"] = await MeasureConvergence(catalog, root, "e2e-create", (path, _) => File.WriteAllText(path, "create"), expected: 1);
        string createPath = Path.Combine(root, "e2e-create.md");
        File.Delete(createPath);
        report["delete_ms"] = await MeasureConvergence(catalog, root, "e2e-create", (_, _) => { }, expected: 0);

        string renameSource = Path.Combine(root, "e2e-rename-source.md");
        string renameTarget = Path.Combine(root, "e2e-rename-target.md");
        File.WriteAllText(renameSource, "rename");
        await Until(() => catalog.Search(new SearchRequest("e2e-rename-source")).Records.Count == 1, "rename source create");
        report["rename_ms"] = await MeasureConvergence(catalog, root, "e2e-rename-target", (_, _) => File.Move(renameSource, renameTarget), expected: 1);

        string moveDirectory = Path.Combine(root, "fixture_dir_000");
        string moveSource = Path.Combine(root, "e2e-move-source.md");
        string moveTarget = Path.Combine(moveDirectory, "e2e-move-target.md");
        File.WriteAllText(moveSource, "move");
        await Until(() => catalog.Search(new SearchRequest("e2e-move-source")).Records.Count == 1, "move source create");
        report["move_ms"] = await MeasureConvergence(catalog, root, "e2e-move-target", (_, _) => File.Move(moveSource, moveTarget), expected: 1);
        Check(catalog.Search(new SearchRequest("e2e-move-target")).Records.Single().FullPath == moveTarget, "move path is current");
        File.Delete(moveTarget);
        await Until(() => catalog.Search(new SearchRequest("e2e-move-target")).Records.Count == 0, "move cleanup");
        File.Delete(renameTarget);
        await Until(() => catalog.Search(new SearchRequest("e2e-rename-target")).Records.Count == 0, "rename cleanup");

        await catalog.WaitForIdleAsync(TimeSpan.FromMinutes(2));
        report["idle_private_bytes"] = Process.GetCurrentProcess().PrivateMemorySize64;
    }

    // Restart catch-up: mutate the dedicated tree while the catalog is stopped.
    string stoppedCreate = Path.Combine(root, "restart-created.md");
    File.WriteAllText(stoppedCreate, "restart");
    await using (var restarted = FileSystemCatalog.Open(root, store))
    {
        await restarted.Ready;
        await Until(() => restarted.Search(new SearchRequest("restart-created")).Records.Count == 1, "restart catch-up");
        report["restart_catchup"] = true;
    }

    // Fail-safe recovery: the next open must rebuild rather than serve a corrupt index.
    File.WriteAllBytes(store, [0, 1, 2, 3, 4, 5]);
    await using (var recovered = FileSystemCatalog.Open(root, store))
    {
        await recovered.Ready;
        await Until(() => recovered.Search(new SearchRequest("restart-created")).Records.Count == 1, "corrupt store recovery");
        report["corrupt_store_recovery"] = true;
    }
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
    try { if (Directory.Exists(root)) Directory.Delete(root, true); }
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

static async Task<double> MeasureConvergence(FileSystemCatalog catalog, string root, string query, Action<string, string> mutate, int expected)
{
    string path = Path.Combine(root, query + ".md");
    var watch = Stopwatch.StartNew();
    mutate(path, root);
    await Until(() => catalog.Search(new SearchRequest(query)).Records.Count == expected, query + " convergence");
    watch.Stop();
    return watch.Elapsed.TotalMilliseconds;
}

static async Task Until(Func<bool> condition, string name)
{
    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
    while (!condition())
    {
        timeout.Token.ThrowIfCancellationRequested();
        await Task.Delay(25, timeout.Token);
    }
}

using System.Text;
using PersonalRag.FilenameSearch;

string work = Path.Combine(Path.GetTempPath(), "personalrag-filename-tests-" + Guid.NewGuid().ToString("N"));
string root = Path.Combine(work, "root");
string store = Path.Combine(work, "store", "index.routec");
Directory.CreateDirectory(root);
int checks = 0;
void Check(bool condition, string name)
{
    checks++;
    if (!condition) throw new Exception("FAIL: " + name);
}

try
{
    string nested = Path.Combine(root, "設計_2026", "deep");
    Directory.CreateDirectory(nested);
    File.WriteAllText(Path.Combine(root, "README.txt"), "filename only");
    File.WriteAllText(Path.Combine(root, "report_2026.XLSX"), "content is ignored");
    File.WriteAllText(Path.Combine(nested, "café.log"), "x", new UTF8Encoding(false));
    File.WriteAllText(Path.Combine(nested, "cafe\u0301-nfd.log"), "x", new UTF8Encoding(false));
    File.WriteAllText(Path.Combine(nested, "needle_kappa_9901.txt"), "needle");

    await using (var catalog = FileSystemCatalog.Open(root, store))
    {
        await catalog.Ready;
        await catalog.WaitForIdleAsync(TimeSpan.FromSeconds(10));
        Check(catalog.Records.Count >= 8, "files and directories are collected");
        Check(catalog.Search(new SearchRequest("README")).Records.Count == 1, "case-insensitive filename");
        Check(catalog.Search(new SearchRequest("readme", CaseSensitive: true)).Records.Count == 0, "case-sensitive filename");
        Check(catalog.Search(new SearchRequest("*.log")).Records.Count == 2, "wildcard filename");
        Check(catalog.Search(new SearchRequest("設計 2026")).Records.Count >= 1, "AND filename");
        Check(catalog.Search(new SearchRequest("café")).Records.Count == 2, "NFC/NFD filename equivalence");
        Check(catalog.Search(new SearchRequest("\\", SearchScope.FullPath)).Records.Count == catalog.Records.Count, "one-code-point path scan");
        Check(catalog.Search(new SearchRequest("absent_ASTRA_9f23c751")).Records.Count == 0, "zero hit");
        Check(catalog.Search(new SearchRequest("needle", Limit: 1, RequestId: 42)).RequestId == 42, "request id is preserved");

        string created = Path.Combine(root, "live-created.md");
        File.WriteAllText(created, "live");
        await Until(() => catalog.Search(new SearchRequest("live-created")).Records.Count == 1, "create convergence");
        string renamed = Path.Combine(root, "live-renamed.md");
        File.Move(created, renamed);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.Count == 1 && catalog.Search(new SearchRequest("live-created")).Records.Count == 0, "rename convergence");
        int renamedId = catalog.Search(new SearchRequest("live-renamed")).Records.Single().FileId;
        string movedDirectory = Path.Combine(root, "moved");
        Directory.CreateDirectory(movedDirectory);
        await Until(() => catalog.Status == "Ready", "directory create reconcile");
        string moved = Path.Combine(movedDirectory, "live-renamed.md");
        File.Move(renamed, moved);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.SingleOrDefault()?.FullPath == moved, "move convergence");
        Check(catalog.Search(new SearchRequest("live-renamed")).Records.Single().FileId == renamedId, "rename and move preserve id");
        File.Delete(moved);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.Count == 0, "delete convergence");
        await catalog.WaitForIdleAsync(TimeSpan.FromSeconds(10));
        Check(File.Exists(store), "store persisted");
        Check(!Directory.EnumerateFiles(Path.GetDirectoryName(store)!, "*.tmp").Any(), "temporary stores are cleaned");
    }

    string stoppedMove = Path.Combine(root, "stopped-move.txt");
    File.WriteAllText(stoppedMove, "restart");
    await using (var first = FileSystemCatalog.Open(root, store))
    {
        await first.Ready;
        await first.WaitForIdleAsync(TimeSpan.FromSeconds(10));
    }
    string restarted = Path.Combine(root, "restarted.txt");
    File.Move(stoppedMove, restarted);
    await using (var second = FileSystemCatalog.Open(root, store))
    {
        await second.Ready;
        await Until(() => second.Search(new SearchRequest("restarted")).Records.Count == 1, "restart catch-up");
        Check(second.Search(new SearchRequest("stopped-move")).Records.Count == 0, "restart removes stale path");
    }

    // A corrupt visible store must fail safe by rebuilding from the filesystem.
    File.WriteAllBytes(store, [0, 1, 2, 3, 4]);
    await using (var recovered = FileSystemCatalog.Open(root, store))
    {
        await recovered.Ready;
        await Until(() => recovered.Search(new SearchRequest("restarted")).Records.Count == 1, "corrupt store recovery");
    }
    Console.WriteLine($"PASS FilenameSearch {checks} checks");
}
finally
{
    try { if (Directory.Exists(work)) Directory.Delete(work, true); }
    catch (IOException) { }
}

static async Task Until(Func<bool> condition, string name)
{
    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
    while (!condition())
    {
        timeout.Token.ThrowIfCancellationRequested();
        await Task.Delay(25, timeout.Token);
    }
}

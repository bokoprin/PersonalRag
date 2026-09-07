using System.Text;
using PersonalRag.FilenameSearch;

string work = Path.Combine(Path.GetTempPath(), "personalrag-filename-tests-" + Guid.NewGuid().ToString("N"));
string root = Path.Combine(work, "root");
string store = Path.Combine(work, "store", "index.manifest");
Directory.CreateDirectory(root);
int checks = 0;
void Check(bool condition, string name)
{
    checks++;
    if (!condition) throw new Exception("FAIL: " + name);
}

var unicodeVectors = new (string Target, string Query, string Name)[]
{
    ("Straße", "STRASSE", "full fold sharp s"),
    ("ẞ", "ss", "full fold capital sharp s"),
    ("oﬃce", "office", "full fold ligature ffi"),
    ("ΟΣ", "ος", "full fold final sigma"),
    ("Kelvin", "kelvin", "full fold Kelvin sign"),
    ("İstanbul", "i\u0307stanbul", "full fold dotted I")
};
foreach (var vector in unicodeVectors)
    Check(global::FilenameSearch.Core.FilenameSemantics.Matches(vector.Target, vector.Query, false), vector.Name);
Check(global::FilenameSearch.Core.FilenameSemantics.Matches("my_report_2026.xlsx_backup", "report_*.xlsx", false),
    "wildcard substring with suffix");
Check(global::FilenameSearch.Core.FilenameSemantics.Matches("abcXYZdef", "X?Z", false),
    "wildcard single scalar");
Check(global::FilenameSearch.Core.FilenameSemantics.Matches("alpha", "*", false),
    "wildcard zero or more scalars");
Check(!global::FilenameSearch.Core.FilenameSemantics.Matches("Straße", "STRASSE", true),
    "case-sensitive sharp s remains distinct");

try
{
    string nested = Path.Combine(root, "設計_2026", "deep");
    Directory.CreateDirectory(nested);
    File.WriteAllText(Path.Combine(root, "README.txt"), "filename only");
    File.WriteAllText(Path.Combine(root, "prefix_report_2026.xlsx_suffix"), "content is ignored");
    string nfcPath = Path.Combine(nested, "café.log");
    string nfdPath = Path.Combine(nested, "cafe\u0301-nfd.log");
    File.WriteAllText(nfcPath, "x", new UTF8Encoding(false));
    File.WriteAllText(nfdPath, "x", new UTF8Encoding(false));
    File.WriteAllText(Path.Combine(nested, "needle_kappa_9901.txt"), "needle");
    File.WriteAllText(Path.Combine(root, "oﬃce.txt"), "ligature");

    await using (var catalog = FileSystemCatalog.Open(root, store))
    {
        long firstGeneration = catalog.Generation;
        var changeBatches = new List<CatalogChangeBatch>();
        catalog.Changed += batch => { lock (changeBatches) changeBatches.Add(batch); };
        await catalog.Ready;
        await catalog.WaitForIdleAsync(TimeSpan.FromSeconds(20));

        bool secondWriterRejected = false;
        try
        {
            await using FileSystemCatalog unexpectedSecondWriter = FileSystemCatalog.Open(root, store);
        }
        catch (IOException)
        {
            secondWriterRejected = true;
        }
        Check(secondWriterRejected, "writer lease rejects a second catalog on the same store");

        Check(catalog.Records.Count >= 9, "files and directories are collected");
        CatalogSnapshot initialSnapshot = catalog.GetSnapshot();
        Check(initialSnapshot.Records.Count == catalog.RecordCount, "catalog exposes full initial snapshot for future content indexing");
        Check(initialSnapshot.SourceGenerations.Values.Single() == catalog.Generation, "snapshot exposes source generation");
        Check(catalog.Search(new SearchRequest("README")).Records.Count == 1, "case-insensitive filename");
        Check(catalog.Search(new SearchRequest("readme", CaseSensitive: true)).Records.Count == 0, "case-sensitive filename");
        Check(catalog.Search(new SearchRequest("*.log")).Records.Count == 2, "wildcard filename");
        Check(catalog.Search(new SearchRequest("report_*.xlsx")).Records.Count == 1,
            "wildcard is substring semantics rather than whole-string glob");
        Check(catalog.Search(new SearchRequest("設計 2026")).Records.Count >= 1, "AND filename");
        Check(catalog.Search(new SearchRequest("café")).Records.Count == 2, "NFC/NFD search equivalence");

        var cafeHits = catalog.Search(new SearchRequest("café", Limit: 10)).Records;
        Check(cafeHits.Select(r => r.FullPath).Distinct(StringComparer.Ordinal).Count() == 2,
            "exact NFC/NFD filesystem paths remain distinct");
        Check(cafeHits.All(r => File.Exists(r.FullPath)), "returned exact Unicode paths are openable");
        Check(catalog.Search(new SearchRequest("office")).Records.Count == 1,
            "Unicode full case folding maps ligature ffi");
        int pathHits = catalog.Search(new SearchRequest("\\", SearchScope.FullPath)).Records.Count;
        Check(pathHits == catalog.Records.Count,
            $"one-code-point path scan (hits={pathHits}, records={catalog.Records.Count})");
        Check(catalog.Search(new SearchRequest("absent_ASTRA_9f23c751")).Records.Count == 0, "zero hit");
        Check(catalog.Search(new SearchRequest("needle", Limit: 1, RequestId: 42)).RequestId == 42,
            "request id is preserved");

        FilenameSearchResult beforeDelta = catalog.Search(new SearchRequest("needle_kappa_9901"));
        Check(!beforeDelta.UsedScan, "rare query is indexed before live update");

        string unrelated = Path.Combine(root, "unrelated-live.txt");
        File.WriteAllText(unrelated, "live");
        await Until(() => catalog.Search(new SearchRequest("unrelated-live")).Records.Count == 1,
            "create convergence");
        FilenameSearchResult afterDelta = catalog.Search(new SearchRequest("needle_kappa_9901"));
        Check(!afterDelta.UsedScan,
            "one live update does not force Route C immutable base into full scan");

        await Until(() => catalog.Generation > firstGeneration, "catalog generation advances");
        await Until(() =>
        {
            lock (changeBatches)
                return changeBatches.Any(batch => batch.Changes.Any(c => c.Kind == CatalogChangeKind.Added));
        }, "typed change feed reports added record");
        lock (changeBatches)
        {
            CatalogChangeBatch addedBatch = changeBatches.Single(batch =>
                batch.Changes.Any(c => c.Kind == CatalogChangeKind.Added));
            Check(addedBatch.SourceId is not null, "typed added event identifies its source");
            Check(addedBatch.SourceGeneration > 0, "typed added event reports source generation");
        }

        string createdDirectory = Path.Combine(root, "live-created-directory");
        Directory.CreateDirectory(createdDirectory);
        await Until(() => catalog.Search(new SearchRequest("live-created-directory")).Records
            .Any(record => record.IsDirectory), "directory create convergence");
        await Until(() =>
        {
            lock (changeBatches)
                return changeBatches.Any(batch => batch.Changes.Any(change =>
                    change.Kind == CatalogChangeKind.Added &&
                    change.After is { IsDirectory: true } after &&
                    after.FullPath.Equals(createdDirectory, StringComparison.Ordinal)));
        }, "typed directory added event");
        Directory.Delete(createdDirectory);
        await Until(() => catalog.Search(new SearchRequest("live-created-directory")).Records.Count == 0,
            "directory delete convergence");

        string renamed = Path.Combine(root, "live-renamed.md");
        File.Move(unrelated, renamed);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.Count == 1 &&
                          catalog.Search(new SearchRequest("unrelated-live")).Records.Count == 0,
            "rename convergence");
        await Until(() =>
        {
            lock (changeBatches)
                return changeBatches.Any(batch => batch.Changes.Any(change =>
                    change.Kind == CatalogChangeKind.Renamed &&
                    change.After?.FullPath.Equals(renamed, StringComparison.Ordinal) == true));
        }, "typed rename event");
        int renamedId = catalog.Search(new SearchRequest("live-renamed")).Records.Single().FileId;
        FileKey renamedKey = catalog.Search(new SearchRequest("live-renamed")).Records.Single().Key;

        string movedDirectory = Path.Combine(root, "moved");
        Directory.CreateDirectory(movedDirectory);
        await catalog.WaitForIdleAsync(TimeSpan.FromSeconds(20));
        string moved = Path.Combine(movedDirectory, "live-renamed.md");
        File.Move(renamed, moved);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.SingleOrDefault()?.FullPath == moved,
            "move convergence");
        await Until(() =>
        {
            lock (changeBatches)
                return changeBatches.Any(batch => batch.Changes.Any(change =>
                    change.Kind == CatalogChangeKind.Moved &&
                    change.After?.FullPath.Equals(moved, StringComparison.Ordinal) == true));
        }, "typed move event");
        FilenameRecord movedRecord = catalog.Search(new SearchRequest("live-renamed")).Records.Single();
        Check(movedRecord.FileId == renamedId, "rename and move preserve internal id");
        if (renamedKey.IsNative)
            Check(movedRecord.Key == renamedKey, "native FileKey survives rename/move");

        string deleteTree = Path.Combine(root, "delete-tree");
        Directory.CreateDirectory(Path.Combine(deleteTree, "child"));
        File.WriteAllText(Path.Combine(deleteTree, "child", "descendant-needle.txt"), "x");
        await Until(() => catalog.Search(new SearchRequest("descendant-needle")).Records.Count == 1,
            "directory delete fixture indexed");
        Directory.Delete(deleteTree, recursive: true);
        await Until(() => catalog.Search(new SearchRequest("descendant-needle")).Records.Count == 0,
            "directory delete reconciles descendants without requiring another watcher event");

        File.Delete(moved);
        await Until(() => catalog.Search(new SearchRequest("live-renamed")).Records.Count == 0,
            "delete convergence");
        await catalog.WaitForIdleAsync(TimeSpan.FromSeconds(20));
        Check(File.Exists(store), "generation manifest persisted");
        Check(FileSystemCatalog.PersistentBytes(store) > new FileInfo(store).Length,
            "persistent accounting includes generation sidecar");
    }

    string stoppedMove = Path.Combine(root, "stopped-move.txt");
    File.WriteAllText(stoppedMove, "restart");
    await using (var first = FileSystemCatalog.Open(root, store))
    {
        await first.Ready;
        await first.WaitForIdleAsync(TimeSpan.FromSeconds(20));
    }
    string restarted = Path.Combine(root, "restarted.txt");
    File.Move(stoppedMove, restarted);
    await using (var second = FileSystemCatalog.Open(root, store))
    {
        await second.Ready;
        await Until(() => second.Search(new SearchRequest("restarted")).Records.Count == 1, "restart catch-up");
        Check(second.Search(new SearchRequest("stopped-move")).Records.Count == 0, "restart removes stale path");
    }

    string stoppedModified = Path.Combine(root, "stopped-modified.txt");
    File.WriteAllText(stoppedModified, "before");
    await using (var beforeModification = FileSystemCatalog.Open(root, store))
    {
        await beforeModification.Ready;
        await beforeModification.WaitForIdleAsync(TimeSpan.FromSeconds(20));
    }
    DateTime beforeTime = File.GetLastWriteTimeUtc(stoppedModified);
    File.WriteAllText(stoppedModified, "after-restart-with-a-different-size");
    File.SetLastWriteTimeUtc(stoppedModified, beforeTime.AddSeconds(2));
    ulong modifiedSize = (ulong)new FileInfo(stoppedModified).Length;
    DateTime modifiedTime = File.GetLastWriteTimeUtc(stoppedModified);
    await using (var afterModification = FileSystemCatalog.Open(root, store))
    {
        await afterModification.Ready;
        await Until(() =>
        {
            FilenameRecord? hit = afterModification.Search(new SearchRequest("stopped-modified")).Records.SingleOrDefault();
            return hit?.SizeBytes == modifiedSize && hit.ModifiedUtc == modifiedTime;
        }, "restart detects modified metadata");
        FilenameRecord refreshed = afterModification.Search(new SearchRequest("stopped-modified")).Records.Single();
        Check(refreshed.SizeBytes == modifiedSize, "restart refreshes changed file size");
        Check(refreshed.ModifiedUtc == modifiedTime, "restart refreshes changed modified time");
    }

    // A volume catalog rooted above the shared multi-volume store must exclude the entire
    // shared store root, not only its own per-volume subdirectory. Otherwise another volume's
    // index writes can feed back into the C:\ catalog.
    string sharedStoreRoot = Path.Combine(root, ".shared-volume-store");
    string ownStore = Path.Combine(sharedStoreRoot, "own", "index.manifest");
    string siblingStoreFile = Path.Combine(sharedStoreRoot, "other", "foreign-index.routec");
    Directory.CreateDirectory(Path.GetDirectoryName(siblingStoreFile)!);
    File.WriteAllText(siblingStoreFile, "must never be indexed");
    await using (var excludedStoreCatalog = FileSystemCatalog.Open(root, ownStore, [sharedStoreRoot]))
    {
        await excludedStoreCatalog.Ready;
        await excludedStoreCatalog.WaitForIdleAsync(TimeSpan.FromSeconds(20));
        Check(excludedStoreCatalog.Search(new SearchRequest("foreign-index")).Records.Count == 0,
            "shared multi-volume store root is excluded from self-indexing");
    }

    // Corrupt only the visible manifest. The sidecar may still contain old generations;
    // opening must refuse them without a trusted manifest and rebuild from the filesystem.
    File.WriteAllBytes(store, [0, 1, 2, 3, 4]);
    await using (var recovered = FileSystemCatalog.Open(root, store))
    {
        await recovered.Ready;
        await Until(() => recovered.Search(new SearchRequest("restarted")).Records.Count == 1,
            "corrupt store recovery");
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
    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
    while (!condition())
    {
        timeout.Token.ThrowIfCancellationRequested();
        await Task.Delay(25, timeout.Token);
    }
}

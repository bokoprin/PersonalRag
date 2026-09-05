using Astra.Core;
using System.Text;
using System.Text.RegularExpressions;

int checks = 0;
void Check(bool condition, string name) { checks++; if (!condition) throw new Exception("FAIL: " + name); }
void Throws<T>(Action action, string name) where T : Exception
{ try { action(); } catch (T) { checks++; return; } throw new Exception("Expected " + typeof(T).Name + ": " + name); }
string work = Path.Combine(Path.GetTempPath(), "astra-tests-" + Guid.NewGuid().ToString("N"));
string root = Path.Combine(work, "source"), store = Path.Combine(work, "index");
Directory.CreateDirectory(root);
try
{
    var corpus = new Dictionary<string, string>
    {
        ["main source.txt"] = "hello WORLD\n日本語 日本語\nconfiguration=value\nth in er 00\n",
        ["日本語😀.md"] = "PersonalRag\nHELLO world\nrequest_123\nERROR timeout\n",
        ["empty.txt"] = "",
        ["greek.txt"] = "Σ σ ς K K k ſ S s İ i ı I\n𐐀𐐨\nxxKyy xxſyy xxΣyy xxςyy xxİyy xxıyy\n",
        ["utf16le.txt"] = "日本語\nHello\n",
        ["utf16be.txt"] = "日本語\nHello\n",
        ["no-bom-le.txt"] = "Alpha\nBeta\n",
        ["no-bom-be.txt"] = "Alpha\nBeta\n",
        ["huge.log"] = string.Concat(Enumerable.Repeat("needle needle\n", 25000)),
        ["escape[1].txt"] = "a.b (x) [v] $cash + plus\n<hello>&\n"
    };
    foreach (var pair in corpus)
    {
        Encoding enc = pair.Key switch
        {
            "utf16le.txt" => new UnicodeEncoding(false, true, true), "utf16be.txt" => new UnicodeEncoding(true, true, true),
            "no-bom-le.txt" => new UnicodeEncoding(false, false, true), "no-bom-be.txt" => new UnicodeEncoding(true, false, true),
            _ => new UTF8Encoding(false, true)
        };
        File.WriteAllText(Path.Combine(root, pair.Key), pair.Value, enc);
    }
    File.WriteAllBytes(Path.Combine(root, "binary.bin"), [0, 1, 2, 3, 0xff, 0xfe]);
    File.WriteAllBytes(Path.Combine(root, "bad-utf8.txt"), [0xc3, 0x28]);
    File.WriteAllText(Path.Combine(root, "binary-control.txt"), "prefix\0suffix", new UTF8Encoding(false, true));
    File.WriteAllText(Path.Combine(root, "corrupt.docx"), "this is not a zip package", new UTF8Encoding(false, true));
    var snapshot = new IndexBuilder().Build(root);
    Check(snapshot.Files.Length == 14, "all filenames, including unsearchable files");
    Check(snapshot.Files.Count(f => f.Unsearchable != null) == 4, "binary malformed text and corrupt document rejected");
    Check(snapshot.Files.Single(f => Path.GetFileName(f.Path) == "binary-control.txt").Unsearchable != null, "binary control text isolated");
    Check(snapshot.Files.Single(f => Path.GetFileName(f.Path) == "corrupt.docx").Unsearchable != null, "corrupt DOCX isolated");
    IndexStore.Save(store, snapshot); snapshot = IndexStore.Load(store);
    using (var lease = IndexStore.AcquireWriter(store))
        Throws<IOException>(() => IndexStore.Save(store, snapshot), "concurrent writer rejected");
    string abandoned = Path.Combine(store, "snapshot-" + Guid.NewGuid().ToString("N") + ".tmp");
    File.WriteAllText(abandoned, "interrupted write");
    IndexStore.Save(store, snapshot);
    Check(!File.Exists(abandoned), "abandoned write cleaned after durable save");
    var engine = new SearchEngine(snapshot);
    var queries = new[] { "hello", "日本語", "th", "in", "er", "00", "absent", "needle", "a.b", "Σ", "σ", "ς", "K", "k", "ſ", "S", "İ", "i", "ı", "𐐀", "𐐨", "configuration" };
    foreach (bool cs in new[] { false, true })
    foreach (string query in queries)
    {
        var request = new SearchRequest(ContentQuery: query, CaseSensitive: cs);
        var expected = corpus.Where(p => p.Value.Split('\n').Any(line => line.Contains(query, cs ? StringComparison.Ordinal : StringComparison.OrdinalIgnoreCase)))
            .Select(p => p.Key).Order().ToArray();
        var actual = engine.Search(request).Rows.Select(r => Path.GetFileName(r.File.Path)).Order().ToArray();
        Check(expected.SequenceEqual(actual), $"literal oracle {query} / {cs}");
        foreach (var row in engine.Search(request).Rows)
        {
            long expectedCount = 0;
            foreach (string line in corpus[Path.GetFileName(row.File.Path)].Split('\n'))
            {
                int index = 0;
                while ((index = line.IndexOf(query, index, cs ? StringComparison.Ordinal : StringComparison.OrdinalIgnoreCase)) >= 0)
                { expectedCount++; index += query.Length; }
            }
            Check(engine.GetHits(row.File.Path, request, countAll: true).Total == expectedCount, "exact hit count");
        }
    }
    foreach (var query in new[] { "*.txt", "main source", "日本語", "*😀*", "escape[1].txt", "?????.txt", "absent", "" })
    {
        var expected = snapshot.Files.Where(f => query.Split(' ', StringSplitOptions.RemoveEmptyEntries).All(token =>
            token.IndexOfAny(['*', '?']) >= 0 ? Glob(Path.GetFileName(f.Path).ToUpperInvariant(), token.ToUpperInvariant()) :
            Path.GetFileName(f.Path).Contains(token, StringComparison.OrdinalIgnoreCase))).Select(f => f.Path).ToArray();
        Check(engine.Search(new SearchRequest(FileQuery: query)).Rows.Select(r => r.File.Path).SequenceEqual(expected), "filename independent glob oracle " + query);
    }
    foreach (var query in new[] { "request_[0-9]+", "^ERROR.*timeout", "日本語", "[aeiou]", "^$", "(?=needle)", "a\\.b", "xxkyy", "xxsyy", "xxσyy", "xxiyy", "hello|日本語", "hello?", "hello*", "hello{0}", "request_[0-9]*", "HELLO(?i)" })
    {
        var regex = new Regex(query, RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);
        var expected = corpus.Where(p => ReadLines(p.Value).Any(regex.IsMatch)).Select(p => p.Key).Order().ToArray();
        var actual = engine.Search(new SearchRequest(ContentQuery: query, Mode: ContentMode.Regex)).Rows.Select(r => Path.GetFileName(r.File.Path)).Order().ToArray();
        Check(actual.SequenceEqual(expected), "regex direct oracle " + query);
    }
    Check(engine.Search(new SearchRequest("*.md", "hello")).Rows.Count == 1, "filename AND content");
    Check(engine.Search(new SearchRequest("source", Scope: FileScope.FullPath)).Rows.Count == 14, "full path search");
    Check(engine.Search(new SearchRequest(ContentQuery: "config*value", Mode: ContentMode.Wildcard)).Rows.Count == 1, "content wildcard");
    var huge = engine.Search(new SearchRequest(ContentQuery: "needle"));
    Check(huge.Rows.Count == 1 && !huge.Rows[0].HitsComplete, "huge file first useful row is bounded");
    var hugePath = Path.Combine(root, "huge.log");
    Check(engine.GetHits(hugePath, new SearchRequest(ContentQuery: "needle"), countAll: true).Total == 50000, "50000 hits");
    var tail = engine.GetHits(hugePath, new SearchRequest(ContentQuery: "needle"), 49999);
    Check(tail.Hits.Count == 1 && tail.Complete && tail.Hits[0].Location == "Line 25000", "last hit location");
    var pages = new List<string>(); int offset = 0;
    do { var page = engine.Search(new SearchRequest(), offset, 3); pages.AddRange(page.Rows.Select(r => r.File.Path)); offset = page.NextOffset; if (page.Complete) break; } while (true);
    Check(pages.Count == snapshot.Files.Length && pages.Distinct().Count() == pages.Count, "pagination unique all files");
    Throws<ArgumentException>(() => engine.Search(new SearchRequest(ContentQuery: "[", Mode: ContentMode.Regex)), "invalid query");
    Throws<ArgumentOutOfRangeException>(() => engine.Search(new SearchRequest(), -1), "invalid cursor");
    Throws<OperationCanceledException>(() => engine.Search(new SearchRequest(), cancellationToken: new CancellationToken(true)), "cancel");
    Throws<ArgumentException>(() => IndexStore.Save(Path.Combine(root, "bad-index"), snapshot), "store cannot pollute corpus");
    var changed = Path.Combine(root, "main source.txt");
    File.WriteAllText(changed, "unique_changed_content");
    Check(new SearchEngine(new IndexBuilder().Build(root, previous: snapshot)).Search(new SearchRequest(ContentQuery: "unique_changed_content")).Rows.Count == 1, "catchup replaces changed file signature");
    Check(engine.Search(new SearchRequest(ContentQuery: "configuration")).Rows.Count == 0, "old content never returned");
    File.Delete(changed);
    Check(engine.Search(new SearchRequest("main source")).Rows.Count == 0, "deleted file never returned");
    await using (var runtime = new IndexRuntime(store, snapshot))
    {
        await Until(() => runtime.Status == "Ready", "startup catchup");
        string created = Path.Combine(root, "created.txt");
        File.WriteAllText(created, "first live content");
        await Until(() => runtime.Snapshot.Files.Any(f => f.Path == created), "watch create");
        Check(new SearchEngine(runtime.Snapshot).Search(new SearchRequest(ContentQuery: "first live content")).Rows.Count == 1, "created content searchable");
        File.WriteAllText(created, "second live content longer");
        await Until(() => runtime.Snapshot.Files.Any(f => f.Path == created && f.Size == new FileInfo(created).Length), "watch modify");
        var updated = new SearchEngine(runtime.Snapshot);
        Check(updated.Search(new SearchRequest(ContentQuery: "first live content")).Rows.Count == 0, "watch removes old content");
        Check(updated.Search(new SearchRequest(ContentQuery: "second live content")).Rows.Count == 1, "watch adds new content");
        string renamed = Path.Combine(root, "renamed.txt"); File.Move(created, renamed);
        await Until(() => runtime.Snapshot.Files.Any(f => f.Path == renamed) && !runtime.Snapshot.Files.Any(f => f.Path == created), "watch rename");
        File.Delete(renamed);
        await Until(() => !runtime.Snapshot.Files.Any(f => f.Path == renamed), "watch delete");
    }
    Check(!IndexStore.Load(store).Files.Any(f => f.Path.EndsWith("renamed.txt")), "clean shutdown persists latest snapshot");
    var stable = new IndexBuilder().Build(root); var counting = new CountingExtractor();
    new IndexBuilder(counting).Build(root, previous: stable);
    Check(counting.Count == 0, "restart catchup does not reindex unchanged content");
    string documentRoot = Path.Combine(work, "documents"), documentStore = Path.Combine(work, "document-index");
    DocumentFixtures.Create(documentRoot);
    var documentSnapshot = new IndexBuilder().Build(documentRoot);
    Check(documentSnapshot.Files.Length == 4 && documentSnapshot.Files.All(f => f.Unsearchable is null), "Gate 2 document formats indexed");
    IndexStore.Save(documentStore, documentSnapshot);
    var documentEngine = new SearchEngine(IndexStore.Load(documentStore));
    var documentCases = new[]
    {
        ("GateTwoDocxNeedle", "sample.docx", "Paragraph"),
        ("GateTwoXlsxNeedle", "sample.xlsx", "SearchSheet!B27"),
        ("GateTwoPptxNeedle", "sample.pptx", "Slide 1"),
        ("GateTwoPdfNeedle", "sample.pdf", "Page 1")
    };
    foreach (var (query, file, location) in documentCases)
    {
        var page = documentEngine.Search(new SearchRequest(ContentQuery: query));
        Check(page.Rows.Count == 1 && Path.GetFileName(page.Rows[0].File.Path) == file, "Gate 2 search " + file);
        var hits = documentEngine.GetHits(page.Rows[0].File.Path, new SearchRequest(ContentQuery: query), countAll: true);
        Check(hits.Total == 1 && hits.Hits[0].Location.StartsWith(location, StringComparison.Ordinal), "Gate 2 location " + file);
    }
    await using (var documentRuntime = new IndexRuntime(documentStore, documentSnapshot))
    {
        await Until(() => documentRuntime.Status == "Ready", "Gate 2 runtime ready");
        DocumentFixtures.RewriteDocx(documentRoot, "GateTwoDocxChangedNeedle replacement document text");
        await Until(() => new SearchEngine(documentRuntime.Snapshot).Search(new SearchRequest(ContentQuery: "GateTwoDocxChangedNeedle")).Rows.Count == 1,
            "Gate 2 DOCX live update");
        var changedDocuments = new SearchEngine(documentRuntime.Snapshot);
        Check(changedDocuments.Search(new SearchRequest(ContentQuery: "GateTwoDocxNeedle")).Rows.Count == 0, "Gate 2 old DOCX content removed");
        Check(changedDocuments.Search(new SearchRequest(ContentQuery: "GateTwoDocxChangedNeedle")).Rows.Count == 1, "Gate 2 new DOCX content searchable");
    }
    var persistedDocuments = new SearchEngine(IndexStore.Load(documentStore));
    Check(persistedDocuments.Search(new SearchRequest(ContentQuery: "GateTwoDocxChangedNeedle")).Rows.Count == 1, "Gate 2 document update persisted across restart");
    var bytes = File.ReadAllBytes(Path.Combine(store, "snapshot.astra")); bytes[^1] ^= 1; File.WriteAllBytes(Path.Combine(store, "snapshot.astra"), bytes);
    Throws<InvalidDataException>(() => IndexStore.Load(store), "corruption fails safe");
    string blockRoot = Path.Combine(work, "block-source"), blockStore = Path.Combine(work, "block-index");
    Directory.CreateDirectory(blockRoot);
    var random = new Random(513);
    var blockFiles = Enumerable.Range(0, 513).Select(i =>
    {
        var signature = new byte[i % 5 == 0 ? 65536 : 128]; random.NextBytes(signature);
        return new FileEntry(Path.Combine(blockRoot, $"file_{i:D5}_日本語.txt"), i + 10, DateTime.UtcNow.Ticks, i % 17 == 0 ? "unreadable test" : null, signature);
    }).ToArray();
    IndexStore.Save(blockStore, new IndexSnapshot(blockRoot, blockFiles, DateTime.UtcNow));
    var blockLoaded = IndexStore.Load(blockStore);
    Check(blockLoaded.Files.Length == 513, "parallel block count across 256 boundary");
    for (int attempt = 0; attempt < 5; attempt++)
        Check(IndexStore.Load(blockStore).Files.Length == 513, "parallel block repeated load " + attempt);
    for (int i = 0; i < blockFiles.Length; i++)
        Check(blockLoaded.Files[i].Path == blockFiles[i].Path && blockLoaded.Files[i].Unsearchable == blockFiles[i].Unsearchable &&
            blockLoaded.Files[i].Signature.Span.SequenceEqual(blockFiles[i].Signature.Span), "parallel block content " + i);
    IndexStore.Save(blockStore, new IndexSnapshot(blockRoot, [], DateTime.UtcNow));
    Check(IndexStore.Load(blockStore).Files.Length == 0, "zero-block empty snapshot");
    Console.WriteLine($"PASS {checks} checks");
}
finally { Directory.Delete(work, true); }

static IEnumerable<string> ReadLines(string value)
{ using var reader = new StringReader(value); while (reader.ReadLine() is { } line) yield return line; }

static bool Glob(string text, string pattern)
{
    var dp = new bool[text.Length + 1]; dp[0] = true;
    foreach (char c in pattern)
    {
        var next = new bool[text.Length + 1];
        for (int i = 0; i <= text.Length; i++)
            next[i] = c == '*' ? dp[i] || (i > 0 && next[i - 1]) : i > 0 && dp[i - 1] && (c == '?' || c == text[i - 1]);
        dp = next;
    }
    return dp[^1];
}

static async Task Until(Func<bool> condition, string name)
{
    var timeout = System.Diagnostics.Stopwatch.StartNew();
    while (!condition()) { if (timeout.Elapsed.TotalSeconds > 10) throw new Exception("Timeout: " + name); await Task.Delay(25); }
}

sealed class CountingExtractor : ITextExtractor
{
    public int Count;
    public IEnumerable<TextUnit> Extract(string path, CancellationToken cancellationToken = default)
    { Interlocked.Increment(ref Count); return new TextExtractor().Extract(path, cancellationToken); }
}

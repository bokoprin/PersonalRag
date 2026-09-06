using System.Diagnostics;
using System.Text;
using CoreFileRecord = FilenameSearch.Core.FileRecord;
using CoreQuery = FilenameSearch.Core.FilenameQuery;
using CoreScope = FilenameSearch.Core.FilenameScope;
using RouteCEngine = FilenameSearch.RouteC.RouteCEngine;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Production adapter around the frozen Route C immutable base. Live changes are held in a
/// separately indexed delta plus tombstones; they never call RouteCEngine.Upsert, because the
/// bake-off Route C intentionally falls back to a full scan whenever its own overlay is nonempty.
/// </summary>
public sealed class FilenameSearchEngine : IFilenameSearch
{
    private readonly object gate = new();
    private RouteCEngine baseEngine = new();
    private Dictionary<int, FilenameRecord> baseExact = [];
    private readonly Dictionary<int, FilenameRecord?> delta = [];
    private readonly DeltaIndex deltaIndex = new();
    private bool disposed;

    public IReadOnlyList<FilenameRecord> Records
    {
        get
        {
            lock (gate)
            {
                ThrowIfDisposed();
                return SnapshotRecords();
            }
        }
    }

    public int OverlayCount { get { lock (gate) return delta.Count; } }

    public bool ShouldCompact
    {
        get
        {
            lock (gate)
            {
                int threshold = Math.Max(4_096, Math.Max(1, baseExact.Count / 100));
                return delta.Count >= threshold;
            }
        }
    }

    public void Build(IReadOnlyList<FilenameRecord> records)
    {
        ArgumentNullException.ThrowIfNull(records);
        var exact = records.OrderBy(r => r.FileId).ToArray();
        ValidateExact(exact);
        var next = new RouteCEngine();
        next.Build(exact.Select(ToCore).ToArray());
        lock (gate)
        {
            ThrowIfDisposed();
            baseEngine.Dispose();
            baseEngine = next;
            baseExact = exact.ToDictionary(r => r.FileId);
            delta.Clear();
            deltaIndex.Clear();
        }
    }

    public void LoadBase(string indexPath, IReadOnlyList<FilenameRecord> exactRecords)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(indexPath);
        ArgumentNullException.ThrowIfNull(exactRecords);
        var exact = exactRecords.OrderBy(r => r.FileId).ToArray();
        ValidateExact(exact);
        var next = new RouteCEngine();
        next.Load(Path.GetFullPath(indexPath));
        int[] routeIds = next.FileIds.ToArray();
        int[] exactIds = exact.Select(r => r.FileId).OrderBy(x => x).ToArray();
        if (!routeIds.AsSpan().SequenceEqual(exactIds))
        {
            next.Dispose();
            throw new InvalidDataException("Route C base/index metadata identity mismatch");
        }
        lock (gate)
        {
            ThrowIfDisposed();
            baseEngine.Dispose();
            baseEngine = next;
            baseExact = exact.ToDictionary(r => r.FileId);
            delta.Clear();
            deltaIndex.Clear();
        }
    }

    public void SaveBase(string indexPath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(indexPath);
        lock (gate)
        {
            ThrowIfDisposed();
            if (delta.Count != 0)
                throw new InvalidOperationException("SaveBase requires a compacted engine with an empty delta");
            baseEngine.Save(Path.GetFullPath(indexPath));
        }
    }

    /// <summary>
    /// Compatibility helper for tests/tools that need one compacted snapshot.
    /// Product persistence uses GenerationStore instead.
    /// </summary>
    public void SaveAtomic(string indexPath)
    {
        string full = Path.GetFullPath(indexPath);
        string? parent = Path.GetDirectoryName(full);
        if (parent is null) throw new ArgumentException("Index path has no parent", nameof(indexPath));
        Directory.CreateDirectory(parent);
        string temp = full + "." + Guid.NewGuid().ToString("N") + ".tmp";
        using var compacted = CreateCompacted();
        try
        {
            compacted.SaveBase(temp);
            if (File.Exists(full)) File.Replace(temp, full, null, true);
            else File.Move(temp, full);
        }
        finally
        {
            try { if (File.Exists(temp)) File.Delete(temp); } catch { }
        }
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        request.Validate();
        var watch = Stopwatch.StartNew();
        lock (gate)
        {
            ThrowIfDisposed();
            bool hasWildcard = HasWildcard(request.Query);
            string broadQuery = hasWildcard ? BuildBroadLiteralQuery(request.Query) : request.Query;

            // Route C is a conservative candidate generator. Ask it for the complete lazy
            // candidate set; exact verification below stops as soon as enough FileId-ordered
            // true matches have been collected.
            int routeLimit = 0;

            var route = baseEngine.Search(new CoreQuery(
                broadQuery,
                request.Scope == SearchScope.Filename ? CoreScope.Filename : CoreScope.FullPath,
                request.CaseSensitive,
                routeLimit));

            var merged = new Dictionary<int, FilenameRecord>();
            foreach (CoreFileRecord hit in route.Records)
            {
                if (delta.ContainsKey(hit.FileId)) continue;
                if (!baseExact.TryGetValue(hit.FileId, out FilenameRecord? exact)) continue;
                // Route C is intentionally a conservative candidate engine. Search semantics
                // are owned here so exact path spelling, full Unicode case folding, and
                // substring wildcard behavior have one authoritative implementation.
                if (!Matches(exact, request)) continue;
                merged[exact.FileId] = exact;
                if (request.Limit > 0 && merged.Count >= request.Limit) break;
            }

            IReadOnlyList<FilenameRecord> deltaMatches = deltaIndex.Search(
                request,
                delta.Where(pair => pair.Value is not null)
                    .ToDictionary(pair => pair.Key, pair => pair.Value!));
            foreach (FilenameRecord hit in deltaMatches) merged[hit.FileId] = hit;

            FilenameRecord[] ordered = merged.Values.OrderBy(r => r.FileId).ToArray();
            if (request.Limit > 0 && ordered.Length > request.Limit)
                ordered = ordered[..request.Limit];

            watch.Stop();
            return new FilenameSearchResult(
                ordered,
                watch.Elapsed.TotalMilliseconds,
                route.Candidates + deltaIndex.LastCandidateCount,
                route.UsedScan || deltaIndex.LastUsedScan,
                request.RequestId);
        }
    }

    public void Upsert(FilenameRecord record)
    {
        ArgumentNullException.ThrowIfNull(record);
        lock (gate)
        {
            ThrowIfDisposed();
            delta[record.FileId] = record;
            deltaIndex.Upsert(record);
        }
    }

    public bool Remove(int fileId)
    {
        lock (gate)
        {
            ThrowIfDisposed();
            bool existed = baseExact.ContainsKey(fileId) ||
                (delta.TryGetValue(fileId, out FilenameRecord? current) && current is not null);
            if (!existed) return false;
            delta[fileId] = null;
            deltaIndex.Remove(fileId);
            return true;
        }
    }

    public FilenameSearchEngine CreateCompacted()
    {
        FilenameRecord[] snapshot;
        lock (gate)
        {
            ThrowIfDisposed();
            snapshot = SnapshotRecords();
        }
        var compacted = new FilenameSearchEngine();
        compacted.Build(snapshot);
        return compacted;
    }

    public void Dispose()
    {
        lock (gate)
        {
            if (disposed) return;
            baseEngine.Dispose();
            disposed = true;
        }
        GC.SuppressFinalize(this);
    }

    private FilenameRecord[] SnapshotRecords()
    {
        var result = new Dictionary<int, FilenameRecord>(baseExact);
        foreach ((int id, FilenameRecord? record) in delta)
        {
            if (record is null) result.Remove(id);
            else result[id] = record;
        }
        return result.Values.OrderBy(r => r.FileId).ToArray();
    }

    private static CoreFileRecord ToCore(FilenameRecord record) => new(
        record.FileId,
        record.ParentId,
        record.Name,
        record.FullPath,
        record.SizeBytes,
        record.ModifiedUtc.Ticks,
        record.Flags);

    private static bool Matches(FilenameRecord record, SearchRequest request)
    {
        string target = request.Scope == SearchScope.Filename ? record.Name : record.FullPath;
        return global::FilenameSearch.Core.FilenameSemantics.Matches(target, request.Query, request.CaseSensitive);
    }

    private static bool HasWildcard(string query) =>
        query.AsSpan().IndexOfAny('*', '?') >= 0;

    /// <summary>
    /// Creates a conservative literal AND query for the immutable Route C base. Every true
    /// wildcard match must contain all returned literal segments, so this stage may add false
    /// positives but cannot remove a true result. The exact substring-glob semantics are applied
    /// after Route C returns candidates.
    /// </summary>
    private static string BuildBroadLiteralQuery(string query)
    {
        var literals = new List<string>();
        foreach (string token in query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            if (token.IndexOfAny(['*', '?']) < 0)
            {
                literals.Add(token);
                continue;
            }
            var segment = new StringBuilder();
            foreach (Rune rune in token.EnumerateRunes())
            {
                if (rune.Value is '*' or '?')
                {
                    if (segment.Length > 0) { literals.Add(segment.ToString()); segment.Clear(); }
                }
                else segment.Append(rune.ToString());
            }
            if (segment.Length > 0) literals.Add(segment.ToString());
        }
        return string.Join(' ', literals);
    }

    private static void ValidateExact(IReadOnlyList<FilenameRecord> records)
    {
        if (records.Select(r => r.FileId).Distinct().Count() != records.Count)
            throw new ArgumentException("FileId values must be unique", nameof(records));
        // FileKey identifies the underlying file object. Multiple hard-link directory entries
        // may legitimately share one native FileKey while retaining distinct exact paths/FileIds.
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);

    /// <summary>Small n-gram index for the mutable delta. It keeps normal post-update queries off a full base scan.</summary>
    private sealed class DeltaIndex
    {
        private readonly Dictionary<int, Prepared> current = [];
        private readonly Dictionary<string, HashSet<int>> name = new(StringComparer.Ordinal);
        private readonly Dictionary<string, HashSet<int>> path = new(StringComparer.Ordinal);

        public int LastCandidateCount { get; private set; }
        public bool LastUsedScan { get; private set; }

        public void Clear()
        {
            current.Clear(); name.Clear(); path.Clear();
            LastCandidateCount = 0; LastUsedScan = false;
        }

        public void Remove(int id)
        {
            if (!current.Remove(id, out Prepared previous)) return;
            RemoveKeys(name, previous.NameKeys, id);
            RemoveKeys(path, previous.PathKeys, id);
        }

        public void Upsert(FilenameRecord record)
        {
            Remove(record.FileId);
            string nf = global::FilenameSearch.Core.FilenameSemantics.Normalize(record.Name, false);
            string pf = global::FilenameSearch.Core.FilenameSemantics.Normalize(record.FullPath, false);
            string[] nk = Ngrams(nf).Distinct(StringComparer.Ordinal).ToArray();
            string[] pk = Ngrams(pf).Distinct(StringComparer.Ordinal).ToArray();
            AddKeys(name, nk, record.FileId);
            AddKeys(path, pk, record.FileId);
            current[record.FileId] = new Prepared(record, nk, pk);
        }

        public IReadOnlyList<FilenameRecord> Search(
            SearchRequest request,
            IReadOnlyDictionary<int, FilenameRecord> live)
        {
            if (live.Count == 0)
            {
                LastCandidateCount = 0; LastUsedScan = false; return [];
            }
            string foldedQuery = global::FilenameSearch.Core.FilenameSemantics.Normalize(request.Query, false);
            HashSet<int>? candidates = null;
            foreach (string token in foldedQuery.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
            {
                string[] literalSegments = token.IndexOfAny(['*', '?']) >= 0
                    ? LiteralSegments(token)
                    : [token];
                foreach (string segment in literalSegments)
                {
                    string? key = BestKey(segment);
                    if (key is null) continue;
                    var table = request.Scope == SearchScope.Filename ? name : path;
                    if (!table.TryGetValue(key, out HashSet<int>? ids))
                    {
                        LastCandidateCount = 0; LastUsedScan = false; return [];
                    }
                    candidates = candidates is null ? new HashSet<int>(ids) : IntersectInPlace(candidates, ids);
                    if (candidates.Count == 0)
                    {
                        LastCandidateCount = 0; LastUsedScan = false; return [];
                    }
                }
            }

            IEnumerable<int> idsToCheck;
            if (candidates is null)
            {
                LastUsedScan = true;
                idsToCheck = live.Keys;
                LastCandidateCount = live.Count;
            }
            else
            {
                LastUsedScan = false;
                idsToCheck = candidates;
                LastCandidateCount = candidates.Count;
            }

            var result = new List<FilenameRecord>();
            foreach (int id in idsToCheck)
            {
                if (!live.TryGetValue(id, out FilenameRecord? record)) continue;
                if (Matches(record, request)) result.Add(record);
            }
            return result;
        }

        private static string? BestKey(string segment)
        {
            Rune[] runes = segment.EnumerateRunes().ToArray();
            int length = Math.Min(3, runes.Length);
            if (length == 0) return null;
            var b = new StringBuilder();
            for (int i = 0; i < length; i++) b.Append(runes[i].ToString());
            return b.ToString();
        }

        private static string[] LiteralSegments(string token)
        {
            var result = new List<string>();
            var b = new StringBuilder();
            foreach (Rune r in token.EnumerateRunes())
            {
                if (r.Value is '*' or '?')
                {
                    if (b.Length > 0) { result.Add(b.ToString()); b.Clear(); }
                }
                else b.Append(r.ToString());
            }
            if (b.Length > 0) result.Add(b.ToString());
            return result.ToArray();
        }

        private static IEnumerable<string> Ngrams(string value)
        {
            Rune[] runes = value.EnumerateRunes().ToArray();
            for (int len = 1; len <= 3; len++)
                for (int i = 0; i + len <= runes.Length; i++)
                {
                    var b = new StringBuilder();
                    for (int j = 0; j < len; j++) b.Append(runes[i + j].ToString());
                    yield return b.ToString();
                }
        }

        private static void AddKeys(Dictionary<string, HashSet<int>> table, IEnumerable<string> keys, int id)
        {
            foreach (string key in keys)
            {
                if (!table.TryGetValue(key, out HashSet<int>? set)) table[key] = set = [];
                set.Add(id);
            }
        }

        private static void RemoveKeys(Dictionary<string, HashSet<int>> table, IEnumerable<string> keys, int id)
        {
            foreach (string key in keys)
            {
                if (!table.TryGetValue(key, out HashSet<int>? set)) continue;
                set.Remove(id);
                if (set.Count == 0) table.Remove(key);
            }
        }

        private static HashSet<int> IntersectInPlace(HashSet<int> left, HashSet<int> right)
        {
            left.IntersectWith(right);
            return left;
        }

        private readonly record struct Prepared(FilenameRecord Record, string[] NameKeys, string[] PathKeys);
    }
}

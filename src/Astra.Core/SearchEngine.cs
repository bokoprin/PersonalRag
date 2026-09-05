using System.Diagnostics;

namespace Astra.Core;

public sealed class SearchEngine(IndexSnapshot snapshot, ITextExtractor? extractor = null) : IDeterministicSearch
{
    private readonly ITextExtractor extractor = extractor ?? new TextExtractor();
    public IndexSnapshot Snapshot { get; } = snapshot;
    public SearchPage Search(SearchRequest request, int offset = 0, int limit = 100, CancellationToken cancellationToken = default)
    {
        if (offset < 0 || offset > Snapshot.Files.Length || limit is < 1 or > 1000) throw new ArgumentOutOfRangeException(nameof(offset));
        var watch = Stopwatch.StartNew(); var matcher = new QueryMatcher(request);
        var filter = Signature.Compile(request);
        var rows = new List<SearchRow>(); var warnings = new List<string>(); int next = offset;
        while (next < Snapshot.Files.Length && rows.Count < limit)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var file = Snapshot.Files[next++];
            if (!matcher.MatchesFile(file.Path, file.Name)) continue;
            // A snapshot's candidate data is valid until the watcher publishes its replacement.
            // Source verification below prevents returning stale content from positive candidates.
            if (request.ContentQuery.Length > 0 && !filter(file.Signature)) continue;
            var info = new FileInfo(file.Path);
            if (!info.Exists) continue;
            bool unchanged = info.Length == file.Size && info.LastWriteTimeUtc.Ticks == file.ModifiedUtcTicks;
            if (request.ContentQuery.Length == 0) { rows.Add(new SearchRow(file, 0, true, null)); continue; }
            if (unchanged && file.Unsearchable is not null) { if (warnings.Count < 100) warnings.Add(file.Path + ": " + file.Unsearchable); continue; }
            try
            {
                var hits = GetHits(file.Path, matcher, 0, 1, false, cancellationToken);
                if (hits.Hits.Count > 0) rows.Add(new SearchRow(file, hits.Total, hits.Complete, hits.Hits[0]));
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException or System.Text.RegularExpressions.RegexMatchTimeoutException)
            { if (warnings.Count < 100) warnings.Add(file.Path + ": " + ex.Message); }
        }
        return new SearchPage(rows, next, next == Snapshot.Files.Length, watch.Elapsed.TotalMilliseconds, warnings);
    }

    public HitPage GetHits(string path, SearchRequest request, long offset = 0, int limit = 32, bool countAll = false,
        CancellationToken cancellationToken = default)
    {
        if (offset < 0 || limit is < 1 or > 1000) throw new ArgumentOutOfRangeException(nameof(offset));
        return GetHits(path, new QueryMatcher(request), offset, limit, countAll, cancellationToken);
    }
    private HitPage GetHits(string path, QueryMatcher matcher, long offset, int limit, bool countAll, CancellationToken cancellationToken)
    {
        var hits = new List<Hit>(); long total = 0;
        foreach (var unit in extractor.Extract(path, cancellationToken))
        {
            foreach (var match in matcher.Matches(unit.Text))
            {
                cancellationToken.ThrowIfCancellationRequested();
                if (total >= offset && hits.Count < limit)
                {
                    int start = Math.Max(0, match.Start - 100);
                    int length = Math.Min(unit.Text.Length - start, Math.Max(240, Math.Min(match.Length + 100, 500)));
                    hits.Add(new Hit(total, unit.Location, unit.Text.Substring(start, length), match.Start - start, Math.Min(match.Length, length - (match.Start - start))));
                }
                total++;
                if (!countAll && hits.Count == limit) return new HitPage(hits, total, false);
            }
        }
        return new HitPage(hits, total, true);
    }
}

namespace Astra.Core;

// Each outer group must occur; at least one inner alternative must occur.
// Anything not proven mandatory falls back to exact scanning, never to rejecting a candidate.
public static class RequiredLiterals
{
    public static string[][] For(SearchRequest request)
    {
        string query = request.ContentQuery;
        if (query.Length == 0) return [];
        if (request.Mode == ContentMode.Literal) return [[query]];
        if (request.Mode == ContentMode.Wildcard)
            return query.Split(['*', '?'], StringSplitOptions.RemoveEmptyEntries).Where(s => s.Length >= 2).Select(s => new[] { s }).ToArray();
        if (query.Contains('|')) return [];
        int start = query.StartsWith('^') ? 1 : 0;
        int end = start;
        while (end < query.Length && !".\\^$*+?{}[]()".Contains(query[end])) end++;
        if (end == start) return [];
        string prefix = query[start..end];
        if (end < query.Length && "?*{".Contains(query[end])) prefix = prefix[..^1];
        if (prefix.Length < 2) return [];
        string tail = query[end..];
        if (tail.StartsWith("[0-9]", StringComparison.Ordinal) &&
            (tail.Length == 5 || !"?*{".Contains(tail[5])))
            return [Enumerable.Range(0, 10).Select(n => prefix + (char)('0' + n)).ToArray()];
        return [[prefix]];
    }
}

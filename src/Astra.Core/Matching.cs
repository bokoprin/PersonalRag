using System.Text.RegularExpressions;

namespace Astra.Core;

public sealed class QueryMatcher
{
    private readonly SearchRequest request;
    private readonly (string Token, Regex? Pattern)[] names;
    private readonly Regex? content;
    private readonly StringComparison comparison;
    public QueryMatcher(SearchRequest request)
    {
        this.request = request;
        comparison = request.CaseSensitive ? StringComparison.Ordinal : StringComparison.OrdinalIgnoreCase;
        var options = RegexOptions.CultureInvariant | RegexOptions.Compiled | (request.CaseSensitive ? RegexOptions.None : RegexOptions.IgnoreCase);
        names = request.FileQuery.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries)
            .Select(s => (s, s.IndexOfAny(['*', '?']) < 0 ? null : new Regex("\\A" + Wildcard(s) + "\\z", options, TimeSpan.FromMilliseconds(100))))
            .ToArray();
        if (request.ContentQuery.Length != 0 && request.Mode != ContentMode.Literal)
            content = new Regex(request.Mode == ContentMode.Wildcard ? Wildcard(request.ContentQuery) : request.ContentQuery,
                options, TimeSpan.FromMilliseconds(100));
    }

    private static string Wildcard(string value) => Regex.Escape(value).Replace("\\*", ".*").Replace("\\?", ".");
    public bool MatchesFile(string path)
        => MatchesFile(path, System.IO.Path.GetFileName(path));
    public bool MatchesFile(string path, string filename)
    {
        string name = request.Scope == FileScope.FullPath ? path : filename;
        foreach (var token in names)
            if (!(token.Pattern?.IsMatch(name) ?? name.Contains(token.Token, comparison))) return false;
        return true;
    }

    public IEnumerable<(int Start, int Length)> Matches(string text)
    {
        if (request.ContentQuery.Length == 0) yield break;
        if (content is not null)
        {
            for (Match match = content.Match(text); match.Success; match = match.NextMatch())
                yield return (match.Index, match.Length);
        }
        else
        {
            int start = 0;
            while (start <= text.Length - request.ContentQuery.Length)
            {
                int index = text.IndexOf(request.ContentQuery, start, comparison);
                if (index < 0) yield break;
                yield return (index, request.ContentQuery.Length);
                start = index + request.ContentQuery.Length;
            }
        }
    }
}

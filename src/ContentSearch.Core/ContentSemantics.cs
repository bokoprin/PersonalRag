using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using FilenameSearch.Core;

namespace PersonalRag.ContentSearch.Core;

public static class ContentSemantics
{
    public const string NormalizerVersion = FilenameSemantics.NormalizerVersion;

    public static string NormalizeCandidate(string value) => FilenameSemantics.Normalize(value, caseSensitive: false);

    public static int RuneCount(string value) => value.EnumerateRunes().Count();

    public static IReadOnlyList<ulong> UniqueTrigrams(string value)
    {
        string normalized = NormalizeCandidate(value);
        Rune[] runes = normalized.EnumerateRunes().ToArray();
        if (runes.Length < 3) return Array.Empty<ulong>();

        var set = new HashSet<ulong>();
        for (int i = 0; i <= runes.Length - 3; i++)
            set.Add(PackTrigram(runes[i], runes[i + 1], runes[i + 2]));
        return set.ToArray();
    }

    public static ulong PackTrigram(Rune a, Rune b, Rune c) =>
        ((ulong)a.Value << 42) | ((ulong)b.Value << 21) | (uint)c.Value;

    public static bool IsAscii(string value)
    {
        foreach (Rune rune in value.EnumerateRunes())
            if (rune.Value > 0x7F) return false;
        return true;
    }

    public static string EscapeFtsPhrase(string value) =>
        "\"" + value.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
}

public static class ContentExactVerifier
{
    public static IReadOnlyList<ContentMatch> Verify(
        StoredContentBlock block,
        string text,
        ContentQuery query,
        string backendId,
        bool usedScanFallback)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(query);

        return query.Mode switch
        {
            ContentQueryMode.Substring => VerifySubstring(block, text, query, backendId, usedScanFallback),
            ContentQueryMode.Regex => VerifyRegex(block, text, query, backendId, usedScanFallback),
            _ => throw new ArgumentOutOfRangeException(nameof(query))
        };
    }

    public static IReadOnlyList<ContentMatch> Verify(
        ExtractedBlock block,
        ContentQuery query,
        string backendId,
        bool usedScanFallback)
    {
        var descriptor = new StoredContentBlock(
            block.BlockId,
            block.FileKey,
            block.ExactPath,
            block.BlockOrdinal,
            block.DecodedCharStart,
            block.BaseLine,
            0,
            Encoding.UTF8.GetByteCount(block.Text));
        return Verify(descriptor, block.Text, query, backendId, usedScanFallback);
    }

    private static IReadOnlyList<ContentMatch> VerifySubstring(
        StoredContentBlock block,
        string text,
        ContentQuery query,
        string backendId,
        bool usedScanFallback)
    {
        if (query.Text.Length == 0) return Array.Empty<ContentMatch>();

        if (query.CaseSensitive)
            return FindOrdinal(block, text, query.Text, backendId, usedScanFallback);

        FoldedText foldedText = FoldWithMap(text);
        string foldedQuery = FilenameSemantics.Normalize(query.Text, caseSensitive: false);
        if (foldedQuery.Length == 0) return Array.Empty<ContentMatch>();

        var matches = new List<ContentMatch>();
        int searchFrom = 0;
        while (searchFrom <= foldedText.Text.Length - foldedQuery.Length)
        {
            int foldedIndex = foldedText.Text.IndexOf(foldedQuery, searchFrom, StringComparison.Ordinal);
            if (foldedIndex < 0) break;

            int originalStart = foldedText.OriginalIndex[foldedIndex];
            int foldedEnd = foldedIndex + foldedQuery.Length - 1;
            int originalEnd = foldedText.OriginalIndex[Math.Min(foldedEnd, foldedText.OriginalIndex.Length - 1)];
            int originalLength = Math.Max(1, NextTextElementEnd(text, originalEnd) - originalStart);

            matches.Add(CreateMatch(block, text, originalStart, originalLength, backendId, usedScanFallback));
            searchFrom = foldedIndex + Math.Max(1, foldedQuery.Length);
        }

        return Observe(Deduplicate(matches));
    }

    private static IReadOnlyList<ContentMatch> VerifyRegex(
        StoredContentBlock block,
        string text,
        ContentQuery query,
        string backendId,
        bool usedScanFallback)
    {
        if (query.Text.Length == 0) return Array.Empty<ContentMatch>();

        RegexOptions options = RegexOptions.CultureInvariant | RegexOptions.NonBacktracking;
        if (!query.CaseSensitive) options |= RegexOptions.IgnoreCase;

        var regex = new Regex(query.Text, options, TimeSpan.FromSeconds(2));
        var matches = new List<ContentMatch>();
        foreach (Match match in regex.Matches(text))
        {
            if (!match.Success) continue;
            matches.Add(CreateMatch(block, text, match.Index, Math.Max(1, match.Length), backendId, usedScanFallback));
        }
        return Observe(Deduplicate(matches));
    }

    private static IReadOnlyList<ContentMatch> FindOrdinal(
        StoredContentBlock block,
        string text,
        string query,
        string backendId,
        bool usedScanFallback)
    {
        var matches = new List<ContentMatch>();
        int searchFrom = 0;
        while (searchFrom <= text.Length - query.Length)
        {
            int index = text.IndexOf(query, searchFrom, StringComparison.Ordinal);
            if (index < 0) break;
            matches.Add(CreateMatch(block, text, index, Math.Max(1, query.Length), backendId, usedScanFallback));
            searchFrom = index + Math.Max(1, query.Length);
        }
        return Observe(Deduplicate(matches));
    }

    private static ContentMatch CreateMatch(
        StoredContentBlock block,
        string text,
        int localIndex,
        int matchLength,
        string backendId,
        bool usedScanFallback)
    {
        int line = block.BaseLine;
        int lastLineStart = 0;
        for (int i = 0; i < localIndex && i < text.Length; i++)
        {
            if (text[i] == '\n')
            {
                line++;
                lastLineStart = i + 1;
            }
        }

        int column = localIndex - lastLineStart + 1;
        int snippetStart = Math.Max(0, localIndex - 64);
        int snippetEnd = Math.Min(text.Length, localIndex + matchLength + 96);
        string snippet = text[snippetStart..snippetEnd]
            .Replace("\r", " ", StringComparison.Ordinal)
            .Replace("\n", " ", StringComparison.Ordinal);

        return new ContentMatch(
            block.FileKey,
            block.ExactPath,
            block.DecodedCharStart + localIndex,
            line,
            column,
            matchLength,
            snippet,
            block.BlockId,
            backendId,
            usedScanFallback);
    }

    private static IReadOnlyList<ContentMatch> Deduplicate(List<ContentMatch> matches) =>
        matches
            .GroupBy(m => (m.FileKey, m.DecodedCharOffset, m.MatchLength))
            .Select(g => g.First())
            .ToArray();

    private static IReadOnlyList<ContentMatch> Observe(IReadOnlyList<ContentMatch> matches)
    {
        foreach (var match in matches)
        {
            ContentSearchObservation.Report(match);
        }

        return matches;
    }

    private static FoldedText FoldWithMap(string value)
    {
        if (value.Length == 0) return new FoldedText(string.Empty, Array.Empty<int>());

        var folded = new StringBuilder(value.Length);
        var map = new List<int>(value.Length);
        TextElementEnumerator enumerator = StringInfo.GetTextElementEnumerator(value);
        while (enumerator.MoveNext())
        {
            int originalIndex = enumerator.ElementIndex;
            string element = enumerator.GetTextElement();
            string normalized = element.Normalize(NormalizationForm.FormC);
            string foldedElement = FilenameSemantics.CaseFold(normalized).Normalize(NormalizationForm.FormC);
            folded.Append(foldedElement);
            for (int i = 0; i < foldedElement.Length; i++) map.Add(originalIndex);
        }

        return new FoldedText(folded.ToString(), map.ToArray());
    }

    private static int NextTextElementEnd(string text, int index)
    {
        if (index < 0) return 0;
        if (index >= text.Length) return text.Length;
        string element = StringInfo.GetNextTextElement(text, index);
        return Math.Min(text.Length, index + Math.Max(1, element.Length));
    }

    private sealed record FoldedText(string Text, int[] OriginalIndex);
}

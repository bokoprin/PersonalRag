using System.Globalization;
using System.Text;
using System.Text.Unicode;

namespace FilenameSearch.Core;

/// <summary>Shared filename/path semantics used by all routes. The oracle has an independent implementation.</summary>
public static class FilenameSemantics
{
    public static string Normalize(string value, bool caseSensitive)
    {
        string nfc = value.Normalize(NormalizationForm.FormC);
        return caseSensitive ? nfc : CaseFold(nfc).Normalize(NormalizationForm.FormC);
    }

    public static bool Matches(FileRecord record, FilenameQuery request)
    {
        string target = request.Scope == FilenameScope.Filename ? record.Name : record.FullPath;
        string normalizedTarget = Normalize(target, request.CaseSensitive);
        foreach (string token in request.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            string normalizedToken = Normalize(token, request.CaseSensitive);
            if (normalizedToken.IndexOfAny(['*', '?']) >= 0)
            {
                if (!Glob(normalizedTarget, normalizedToken)) return false;
            }
            else if (!normalizedTarget.Contains(normalizedToken, StringComparison.Ordinal)) return false;
        }
        return true;
    }

    public static bool Glob(string target, string pattern)
    {
        var text = target.EnumerateRunes().ToArray();
        var glob = pattern.EnumerateRunes().ToArray();
        var previous = new bool[text.Length + 1]; previous[0] = true;
        foreach (var rune in glob)
        {
            var next = new bool[text.Length + 1];
            for (int i = 0; i <= text.Length; i++)
            {
                next[i] = rune.Value switch
                {
                    '*' => previous[i] || (i > 0 && next[i - 1]),
                    '?' => i > 0 && previous[i - 1],
                    _ => i > 0 && previous[i - 1] && text[i - 1] == rune
                };
            }
            previous = next;
        }
        return previous[^1];
    }

    public static string CaseFold(string value)
    {
        var builder = new StringBuilder(value.Length);
        foreach (var rune in value.EnumerateRunes())
        {
            switch (rune.Value)
            {
                case 0x00DF: // LATIN SMALL LETTER SHARP S
                case 0x1E9E: builder.Append("ss"); break;
                case 0x0130: builder.Append("i\u0307"); break;
                case 0x0131: builder.Append('ı'); break;
                case 0x017F: builder.Append('s'); break;
                case 0x03C2: builder.Append('σ'); break;
                case 0x212A: builder.Append('k'); break;
                case 0x212B: builder.Append('å'); break;
                case 0xFB00: builder.Append("ff"); break;
                case 0xFB01: builder.Append("fi"); break;
                case 0xFB02: builder.Append("fl"); break;
                case 0xFB03: builder.Append("ffi"); break;
                case 0xFB04: builder.Append("ffl"); break;
                case 0xFB05:
                case 0xFB06: builder.Append("st"); break;
                default: builder.Append(Rune.ToLowerInvariant(rune)); break;
            }
        }
        return builder.ToString();
    }
}

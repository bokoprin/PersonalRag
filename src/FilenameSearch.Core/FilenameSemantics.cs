using System.IO.Compression;
using System.Text;

namespace FilenameSearch.Core;

/// <summary>
/// Frozen filename/path search semantics for the production engine.
/// Search keys are NFC normalized. Case-insensitive matching uses the Unicode 15.1
/// default full case-fold mapping embedded as a compressed generated table.
/// Filesystem identity strings themselves must never be rewritten by this class.
/// </summary>
public static class FilenameSemantics
{
    public const string NormalizerVersion = "unicode-15.1-full-casefold+nfc-v3";
    private static readonly Lazy<IReadOnlyDictionary<int, string>> FullCaseFold =
        new(LoadFullCaseFold, LazyThreadSafetyMode.ExecutionAndPublication);

    public static string Normalize(string value, bool caseSensitive)
    {
        ArgumentNullException.ThrowIfNull(value);
        string nfc = value.Normalize(NormalizationForm.FormC);
        return caseSensitive ? nfc : CaseFold(nfc).Normalize(NormalizationForm.FormC);
    }

    public static bool Matches(FileRecord record, FilenameQuery request)
    {
        ArgumentNullException.ThrowIfNull(record);
        ArgumentNullException.ThrowIfNull(request);
        string target = request.Scope == FilenameScope.Filename ? record.Name : record.FullPath;
        return Matches(target, request.Query, request.CaseSensitive);
    }

    public static bool Matches(string target, string query, bool caseSensitive)
    {
        ArgumentNullException.ThrowIfNull(target);
        ArgumentNullException.ThrowIfNull(query);
        string normalizedTarget = Normalize(target, caseSensitive);
        foreach (string token in query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            string normalizedToken = Normalize(token, caseSensitive);
            bool wildcard = normalizedToken.EnumerateRunes().Any(r => r.Value is '*' or '?');
            if (wildcard)
            {
                if (!GlobSubstring(normalizedTarget, normalizedToken)) return false;
            }
            else if (!normalizedTarget.Contains(normalizedToken, StringComparison.Ordinal))
            {
                return false;
            }
        }
        return true;
    }

    /// <summary>
    /// Wildcards are substring conditions, not whole-string glob conditions.
    /// '?' consumes one Unicode scalar value and '*' consumes zero or more.
    /// </summary>
    public static bool GlobSubstring(string target, string pattern)
    {
        ArgumentNullException.ThrowIfNull(target);
        ArgumentNullException.ThrowIfNull(pattern);
        Rune[] text = target.EnumerateRunes().ToArray();
        Rune[] user = pattern.EnumerateRunes().ToArray();
        Rune star = new('*');
        var glob = new Rune[user.Length + 2];
        glob[0] = star;
        Array.Copy(user, 0, glob, 1, user.Length);
        glob[^1] = star;
        return GlobWhole(text, glob);
    }

    private static bool GlobWhole(Rune[] text, Rune[] glob)
    {
        var previous = new bool[text.Length + 1];
        previous[0] = true;
        foreach (Rune rune in glob)
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
        ArgumentNullException.ThrowIfNull(value);
        IReadOnlyDictionary<int, string> table = FullCaseFold.Value;
        var builder = new StringBuilder(value.Length);
        foreach (Rune rune in value.EnumerateRunes())
        {
            if (table.TryGetValue(rune.Value, out string? folded)) builder.Append(folded);
            else builder.Append(rune.ToString());
        }
        return builder.ToString();
    }

    private static IReadOnlyDictionary<int, string> LoadFullCaseFold()
    {
        byte[] compressed = Convert.FromBase64String(CaseFoldData.GzipBase64);
        using var compressedStream = new MemoryStream(compressed, writable: false);
        using var gzip = new GZipStream(compressedStream, CompressionMode.Decompress);
        using var reader = new BinaryReader(gzip, Encoding.UTF8, leaveOpen: false);
        int count = reader.ReadInt32();
        if (count != CaseFoldData.MappingCount)
            throw new InvalidDataException("Unicode 15.1 case-fold table count mismatch");
        var table = new Dictionary<int, string>(count);
        for (int i = 0; i < count; i++)
        {
            int scalar = reader.ReadInt32();
            int byteCount = reader.ReadByte();
            byte[] utf8 = reader.ReadBytes(byteCount);
            if (utf8.Length != byteCount) throw new EndOfStreamException();
            if (!table.TryAdd(scalar, Encoding.UTF8.GetString(utf8)))
                throw new InvalidDataException("Unicode case-fold table contains a duplicate scalar");
        }
        if (gzip.ReadByte() != -1)
            throw new InvalidDataException("Unicode case-fold table has trailing data");
        return table;
    }
}

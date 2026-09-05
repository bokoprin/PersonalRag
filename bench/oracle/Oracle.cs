using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.Unicode;
using FilenameSearch.Core;
using FilenameSearch.QueryGenerator;

namespace FilenameSearch.Oracle;

public sealed record OracleEntry(string Id, string Class, int ExpectedCount, string ExpectedIdsSha256);
public sealed record OracleDocument(int Version, string CorpusSha256, string QuerySetSha256, IReadOnlyList<OracleEntry> Results);

/// <summary>
/// Independent correctness oracle. It deliberately does not call FilenameSemantics or any route code.
/// </summary>
public static class IndependentOracle
{
    public static OracleDocument Evaluate(CorpusData corpus, QuerySetDocument querySet, string corpusSha, string querySetSha)
    {
        var results = new List<OracleEntry>(querySet.Queries.Count);
        foreach (QuerySpec query in querySet.Queries)
        {
            var ids = new List<int>();
            foreach (FileRecord record in corpus.Records)
            {
                if (Matches(record, query)) ids.Add(record.FileId);
            }
            ids.Sort();
            results.Add(new OracleEntry(query.Id, query.Class, ids.Count, HashIds(ids)));
        }
        return new OracleDocument(1, corpusSha, querySetSha, results);
    }

    public static void Write(string path, OracleDocument document)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        var options = new JsonSerializerOptions
        {
            WriteIndented = true,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) }
        };
        File.WriteAllText(path, JsonSerializer.Serialize(document, options));
    }

    private static bool Matches(FileRecord record, QuerySpec query)
    {
        string value = query.Scope == FilenameScope.Filename ? record.Name : record.FullPath;
        string target = Normalize(value, query.CaseSensitive);
        foreach (string rawToken in query.Query.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            string token = Normalize(rawToken, query.CaseSensitive);
            bool wildcard = token.EnumerateRunes().Any(r => r.Value is '*' or '?');
            if (wildcard ? !Glob(target, token) : target.IndexOf(token, StringComparison.Ordinal) < 0) return false;
        }
        return true;
    }

    private static bool Glob(string target, string pattern)
    {
        Rune[] text = target.EnumerateRunes().ToArray();
        Rune[] glob = pattern.EnumerateRunes().ToArray();
        bool[] previous = new bool[text.Length + 1]; previous[0] = true;
        foreach (Rune rune in glob)
        {
            bool[] next = new bool[text.Length + 1];
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

    private static string Normalize(string value, bool caseSensitive)
    {
        string nfc = value.Normalize(NormalizationForm.FormC);
        return caseSensitive ? nfc : Fold(nfc).Normalize(NormalizationForm.FormC);
    }

    private static string Fold(string value)
    {
        var builder = new StringBuilder(value.Length);
        foreach (Rune rune in value.EnumerateRunes())
        {
            switch (rune.Value)
            {
                case 0x00DF:
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

    private static string HashIds(IReadOnlyList<int> ids)
    {
        using IncrementalHash hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        Span<byte> separator = stackalloc byte[] { (byte)',' };
        foreach (int id in ids)
        {
            byte[] bytes = Encoding.UTF8.GetBytes(id.ToString(System.Globalization.CultureInfo.InvariantCulture));
            hash.AppendData(bytes); hash.AppendData(separator);
        }
        return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant();
    }
}

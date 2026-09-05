namespace Astra.Core;

// Conservative bigram/trigram filters. The original text is always checked before returning a row.
public static class Signature
{
    public static byte[] Allocate(long sourceBytes)
    {
        int size = 128;
        while (size < 65536 && size * 2 <= sourceBytes / 40) size *= 2;
        return new byte[size];
    }
    public static void Add(byte[] bits, string text)
    {
        string folded = text.ToUpperInvariant();
        AddFolded(bits, folded);
        // Regex invariant folding has additional equivalences (for example Kelvin sign -> k).
        string regexFolded = text.ToLowerInvariant().ToUpperInvariant();
        if (regexFolded != folded) AddFolded(bits, regexFolded);
    }
    private static void AddFolded(byte[] bits, string folded)
    {
        for (int i = 0; i + 1 < folded.Length; i++)
        {
            int bit = (int)(Hash(folded[i], folded[i + 1], '\0') & (uint)(bits.Length * 8 - 1));
            bits[bit >> 3] |= (byte)(1 << (bit & 7));
        }
        for (int i = 0; i + 2 < folded.Length; i++)
        {
            uint h = Hash(folded[i], folded[i + 1], folded[i + 2]);
            int bit = (int)(h & (uint)(bits.Length * 8 - 1));
            bits[bit >> 3] |= (byte)(1 << (bit & 7));
        }
    }
    public static bool MayContain(ReadOnlyMemory<byte> bits, SearchRequest request)
        => Compile(request)(bits);
    public static Func<ReadOnlyMemory<byte>, bool> Compile(SearchRequest request)
    {
        var groups = RequiredLiterals.For(request).Select(alternatives => alternatives.Select(value => Hashes(value, request.Mode != ContentMode.Literal)).ToArray()).ToArray();
        return memory =>
        {
            var bits = memory.Span;
            if (bits.Length == 0) return true;
            uint mask = (uint)(bits.Length * 8 - 1);
            foreach (var alternatives in groups)
            {
                bool any = false;
                foreach (var hashes in alternatives)
                {
                    bool all = true;
                    foreach (uint hash in hashes)
                    {
                        int bit = (int)(hash & mask);
                        if ((bits[bit >> 3] & (1 << (bit & 7))) == 0) { all = false; break; }
                    }
                    if (all) { any = true; break; }
                }
                if (!any) return false;
            }
            return true;
        };
    }
    private static uint[] Hashes(string query, bool regexFold)
    {
        if (query.Length < 2) return [];
        string folded = regexFold ? query.ToLowerInvariant().ToUpperInvariant() : query.ToUpperInvariant();
        if (folded.Any(char.IsSurrogate)) return [];
        var hashes = new List<uint>();
        for (int i = 0; i + 1 < folded.Length; i++)
            hashes.Add(Hash(folded[i], folded[i + 1], '\0'));
        for (int i = 0; i + 2 < folded.Length; i++)
            hashes.Add(Hash(folded[i], folded[i + 1], folded[i + 2]));
        return hashes.Distinct().ToArray();
    }
    private static uint Hash(char a, char b, char c) => unchecked(((uint)a * 16777619 ^ b) * 16777619 ^ c) * 2654435761u;
}

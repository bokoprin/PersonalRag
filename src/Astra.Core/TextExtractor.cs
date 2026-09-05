using System.Text;

namespace Astra.Core;

public sealed class TextExtractor : ITextExtractor
{
    public IEnumerable<TextUnit> Extract(string path, CancellationToken cancellationToken = default)
    {
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete,
            65536, FileOptions.SequentialScan);
        var head = new byte[4096];
        int n = stream.Read(head);
        var (encoding, skip) = Detect(head.AsSpan(0, n));
        stream.Position = skip;
        using var reader = new StreamReader(stream, encoding, false, 65536);
        long lineNumber = 0;
        while (reader.ReadLine() is { } line)
        {
            cancellationToken.ThrowIfCancellationRequested();
            foreach (char c in line)
                if (c == '\0' || (char.IsControl(c) && c is not '\t' and not '\f'))
                    throw new InvalidDataException("Binary control characters");
            yield return new TextUnit($"Line {++lineNumber}", line);
        }
    }

    private static (Encoding Encoding, int Skip) Detect(ReadOnlySpan<byte> bytes)
    {
        if (bytes.StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) return (new UTF8Encoding(false, true), 3);
        if (bytes.StartsWith(new byte[] { 0xff, 0xfe, 0, 0 }) || bytes.StartsWith(new byte[] { 0, 0, 0xfe, 0xff }))
            throw new InvalidDataException("UTF-32 is not supported");
        if (bytes.StartsWith(new byte[] { 0xff, 0xfe })) return (new UnicodeEncoding(false, false, true), 2);
        if (bytes.StartsWith(new byte[] { 0xfe, 0xff })) return (new UnicodeEncoding(true, false, true), 2);
        // BOM-less UTF-16 is accepted only when the alternating zero pattern is strong.
        if (bytes.Length >= 4)
        {
            int even = 0, odd = 0, pairs = bytes.Length / 2;
            for (int i = 0; i < pairs * 2; i += 2) { if (bytes[i] == 0) even++; if (bytes[i + 1] == 0) odd++; }
            if (odd > pairs * .6 && even == 0) return (new UnicodeEncoding(false, false, true), 0);
            if (even > pairs * .6 && odd == 0) return (new UnicodeEncoding(true, false, true), 0);
        }
        return (new UTF8Encoding(false, true), 0);
    }
}

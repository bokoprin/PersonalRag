using System.Runtime.CompilerServices;
using System.Text;

namespace PersonalRag.ContentSearch.Core;

public static class TextExtraction
{
    private static readonly HashSet<string> SupportedExtensions = new(StringComparer.OrdinalIgnoreCase)
    {
        ".txt", ".log", ".md", ".csv", ".json", ".xml", ".yaml", ".yml",
        ".ini", ".cfg", ".conf", ".cs", ".c", ".h", ".cpp", ".hpp", ".cc",
        ".py", ".js", ".jsx", ".ts", ".tsx", ".java", ".rs", ".go",
        ".ps1", ".bat", ".cmd", ".sql"
    };

    static TextExtraction()
    {
        Encoding.RegisterProvider(CodePagesEncodingProvider.Instance);
    }

    public static bool IsSupportedPath(string path) => SupportedExtensions.Contains(Path.GetExtension(path));

    public static async Task<ExtractedTextInfo> ProbeAsync(string path, CancellationToken cancellationToken)
    {
        if (!IsSupportedPath(path)) return new(ContentIndexStatus.Unsupported, string.Empty);

        try
        {
            await using var stream = new FileStream(path, FileMode.Open, FileAccess.Read,
                FileShare.ReadWrite | FileShare.Delete, 4096,
                FileOptions.Asynchronous | FileOptions.SequentialScan);

            byte[] sample = new byte[Math.Min(4096, checked((int)Math.Min(stream.Length, 4096L)))];
            int read = 0;
            while (read < sample.Length)
            {
                int n = await stream.ReadAsync(sample.AsMemory(read), cancellationToken).ConfigureAwait(false);
                if (n == 0) break;
                read += n;
            }

            Encoding? encoding = DetectEncoding(sample.AsSpan(0, read), out bool binary);
            if (binary) return new(ContentIndexStatus.Binary, string.Empty);
            if (encoding is null) return new(ContentIndexStatus.DecodeFailed, string.Empty, "Encoding detection failed.");
            return new(ContentIndexStatus.Indexed, encoding.WebName);
        }
        catch (OperationCanceledException) { throw; }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or DecoderFallbackException)
        {
            return new(ContentIndexStatus.DecodeFailed, string.Empty, ex.Message);
        }
    }

    public static async IAsyncEnumerable<ExtractedBlock> EnumerateBlocksAsync(
        ContentDocument document,
        int blockSizeChars,
        int overlapChars,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        if (blockSizeChars <= 0) throw new ArgumentOutOfRangeException(nameof(blockSizeChars));
        if (overlapChars < 0 || overlapChars >= blockSizeChars) throw new ArgumentOutOfRangeException(nameof(overlapChars));
        if (!IsSupportedPath(document.ExactPath)) yield break;

        await using var stream = new FileStream(document.ExactPath, FileMode.Open, FileAccess.Read,
            FileShare.ReadWrite | FileShare.Delete, 64 * 1024,
            FileOptions.Asynchronous | FileOptions.SequentialScan);

        byte[] sample = new byte[Math.Min(4096, checked((int)Math.Min(stream.Length, 4096L)))];
        int sampleRead = 0;
        while (sampleRead < sample.Length)
        {
            int n = await stream.ReadAsync(sample.AsMemory(sampleRead), cancellationToken).ConfigureAwait(false);
            if (n == 0) break;
            sampleRead += n;
        }

        Encoding? encoding = DetectEncoding(sample.AsSpan(0, sampleRead), out bool binary);
        if (binary || encoding is null) yield break;
        stream.Position = 0;

        using var reader = new StreamReader(stream, encoding, true, 64 * 1024, leaveOpen: true);
        char[] buffer = new char[blockSizeChars];
        string overlap = string.Empty;
        long totalDecoded = 0;
        int lineBeforePayload = 1;
        int ordinal = 0;

        while (true)
        {
            cancellationToken.ThrowIfCancellationRequested();
            int count = await reader.ReadAsync(buffer.AsMemory(0, buffer.Length), cancellationToken).ConfigureAwait(false);
            if (count == 0) break;

            string payload = new(buffer, 0, count);
            string text = overlap.Length == 0 ? payload : string.Concat(overlap, payload);
            long start = totalDecoded - overlap.Length;
            int baseLine = Math.Max(1, lineBeforePayload - CountNewLines(overlap));

            yield return new ExtractedBlock(0, document.FileKey, document.ExactPath, ordinal++, start, baseLine, text);

            totalDecoded += count;
            lineBeforePayload += CountNewLines(payload);
            int keep = Math.Min(overlapChars, text.Length);
            overlap = keep == 0 ? string.Empty : text[^keep..];
        }
    }

    private static Encoding? DetectEncoding(ReadOnlySpan<byte> sample, out bool binary)
    {
        binary = false;
        if (sample.StartsWith(new byte[] { 0xEF, 0xBB, 0xBF })) return new UTF8Encoding(true, true);
        if (sample.StartsWith(new byte[] { 0xFF, 0xFE })) return new UnicodeEncoding(false, true, true);
        if (sample.StartsWith(new byte[] { 0xFE, 0xFF })) return new UnicodeEncoding(true, true, true);
        if (sample.Length == 0) return new UTF8Encoding(false, true);

        int nul = 0, controls = 0;
        foreach (byte b in sample)
        {
            if (b == 0) nul++;
            if (b < 0x09 || (b > 0x0D && b < 0x20)) controls++;
        }
        if (nul > sample.Length / 32 || controls > sample.Length / 8)
        {
            binary = true;
            return null;
        }

        try
        {
            var utf8 = new UTF8Encoding(false, true);
            _ = utf8.GetString(sample);
            return utf8;
        }
        catch (DecoderFallbackException)
        {
            try { return Encoding.GetEncoding(932, EncoderFallback.ExceptionFallback, DecoderFallback.ExceptionFallback); }
            catch { return null; }
        }
    }

    private static int CountNewLines(string value)
    {
        int count = 0;
        foreach (char c in value) if (c == '\n') count++;
        return count;
    }
}

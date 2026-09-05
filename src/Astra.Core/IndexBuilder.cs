using System.Collections.Concurrent;

namespace Astra.Core;

public sealed class IndexBuilder(ITextExtractor? extractor = null)
{
    private readonly ITextExtractor extractor = extractor ?? new TextExtractor();
    public IndexSnapshot Build(string root, CancellationToken cancellationToken = default, Action<int>? progress = null, IndexSnapshot? previous = null)
    {
        root = Path.GetFullPath(root);
        if (!Directory.Exists(root)) throw new DirectoryNotFoundException(root);
        var entries = new ConcurrentBag<FileEntry>();
        var old = previous?.Files.ToDictionary(f => f.Path, StringComparer.OrdinalIgnoreCase);
        int done = 0;
        var options = new EnumerationOptions { RecurseSubdirectories = true, IgnoreInaccessible = false,
            AttributesToSkip = FileAttributes.ReparsePoint };
        Parallel.ForEach(Directory.EnumerateFiles(root, "*", options),
            new ParallelOptions { MaxDegreeOfParallelism = Math.Min(8, Environment.ProcessorCount), CancellationToken = cancellationToken },
            path =>
            {
                var info = new FileInfo(path);
                if (old != null && old.TryGetValue(path, out var cached) && cached.Size == info.Length && cached.ModifiedUtcTicks == info.LastWriteTimeUtc.Ticks)
                    entries.Add(cached);
                else entries.Add(ReadEntry(path, cancellationToken));
                int count = Interlocked.Increment(ref done); if (count % 100 == 0) progress?.Invoke(count);
            });
        return new IndexSnapshot(root, entries.OrderBy(e => e.Path, StringComparer.OrdinalIgnoreCase).ToArray(), DateTime.UtcNow);
    }
    public FileEntry ReadEntry(string path, CancellationToken cancellationToken = default)
    {
        var info = new FileInfo(path);
        long size = info.Length, modified = info.LastWriteTimeUtc.Ticks;
        var bits = Signature.Allocate(size);
        string? error = null;
        try
        {
            foreach (var unit in extractor.Extract(path, cancellationToken)) Signature.Add(bits, unit.Text);
            info.Refresh();
            if (!info.Exists || info.Length != size || info.LastWriteTimeUtc.Ticks != modified)
                throw new IOException("File changed while indexing");
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        { error = ex.Message; bits = []; }
        return new FileEntry(path, size, modified, error, bits);
    }
}

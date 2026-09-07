using System.Collections.Concurrent;
using PersonalRag.ContentSearch.Core;

namespace PersonalRag.ContentSearch.Backends.Scan;

public sealed class DirectScanBackend : IContentSearchBackend
{
    private readonly ContentBackendDiagnostics _diagnostics = new();
    private readonly object _gate = new();
    private ContentCorpus? _corpus;
    private Dictionary<ContentFileKey, ContentDocument> _documents = new();

    public string BackendId => "scan";

    public Task BuildAsync(ContentCorpus corpus, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        _corpus = corpus;
        _documents = corpus.Documents.ToDictionary(d => d.FileKey);
        _diagnostics.PersistentBytes = 0;
        _diagnostics.IndexedBlocks = 0;
        _diagnostics.ActiveBlocks = 0;
        return Task.CompletedTask;
    }

    public async Task<IReadOnlyList<ContentMatch>> SearchAsync(ContentQuery query, CancellationToken cancellationToken)
    {
        ContentCorpus corpus = _corpus ?? throw new InvalidOperationException("Backend is not built.");
        var matches = new ConcurrentBag<ContentMatch>();
        long bytesRead = 0;
        long verifiedBlocks = 0;
        long blockId = 0;

        ContentDocument[] documents;
        lock (_gate)
            documents = _documents.Values.OrderBy(d => d.ExactPath, StringComparer.Ordinal).ToArray();

        var options = new ParallelOptions
        {
            CancellationToken = cancellationToken,
            MaxDegreeOfParallelism = Math.Max(1, Math.Min(Environment.ProcessorCount, 8))
        };

        await Parallel.ForEachAsync(documents, options, async (document, ct) =>
        {
            await foreach (ExtractedBlock extracted in TextExtraction.EnumerateBlocksAsync(
                document, corpus.BlockSizeChars, corpus.OverlapChars, ct).ConfigureAwait(false))
            {
                long id = Interlocked.Increment(ref blockId);
                ExtractedBlock block = extracted with { BlockId = id };
                Interlocked.Add(ref bytesRead, System.Text.Encoding.UTF8.GetByteCount(block.Text));
                Interlocked.Increment(ref verifiedBlocks);

                foreach (ContentMatch match in ContentExactVerifier.Verify(block, query, BackendId, usedScanFallback: true))
                    matches.Add(match);
            }
        }).ConfigureAwait(false);

        _diagnostics.SearchCount++;
        _diagnostics.BytesRead += bytesRead;
        _diagnostics.CandidateBlocks += verifiedBlocks;
        _diagnostics.VerifiedBlocks += verifiedBlocks;
        _diagnostics.ScanFallbackCount++;
        _diagnostics.ActiveBlocks = verifiedBlocks;

        return DeduplicateAndSort(matches);
    }

    public Task ApplyChangesAsync(IReadOnlyList<ContentChange> changes, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        lock (_gate)
        {
            foreach (ContentChange change in changes)
            {
                switch (change.Kind)
                {
                    case ContentChangeKind.Added:
                    case ContentChangeKind.Updated:
                    case ContentChangeKind.Renamed:
                    case ContentChangeKind.Moved:
                        if (change.Document is not null) _documents[change.FileKey] = change.Document;
                        break;
                    case ContentChangeKind.Removed:
                        _documents.Remove(change.FileKey);
                        break;
                }
            }
        }

        _diagnostics.UpdateCount += changes.Count;
        return Task.CompletedTask;
    }

    public ContentBackendDiagnostics GetDiagnostics() => _diagnostics.Snapshot();

    public ValueTask DisposeAsync() => ValueTask.CompletedTask;

    private static IReadOnlyList<ContentMatch> DeduplicateAndSort(IEnumerable<ContentMatch> matches) =>
        matches
            .GroupBy(m => (m.FileKey, m.DecodedCharOffset, m.MatchLength))
            .Select(g => g.First())
            .OrderBy(m => m.ExactPath, StringComparer.Ordinal)
            .ThenBy(m => m.DecodedCharOffset)
            .ToArray();
}

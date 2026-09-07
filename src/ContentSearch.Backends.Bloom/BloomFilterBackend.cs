using PersonalRag.ContentSearch.Core;

namespace PersonalRag.ContentSearch.Backends.Bloom;

public sealed class BloomFilterBackend : IContentSearchBackend
{
    private const int FilterBytes = 256;
    private readonly ContentBackendDiagnostics _diagnostics = new();
    private readonly object _gate = new();
    private readonly Dictionary<long, StoredContentBlock> _blocks = new();
    private readonly Dictionary<long, byte[]> _filters = new();
    private readonly Dictionary<ContentFileKey, List<long>> _fileBlocks = new();
    private readonly HashSet<long> _active = new();

    private ContentCorpus? _corpus;
    private ContentBlockStore? _store;
    private FileStream? _filterStream;

    public string BackendId => "bloom";

    public async Task BuildAsync(ContentCorpus corpus, CancellationToken cancellationToken)
    {
        await DisposeStorageAsync().ConfigureAwait(false);

        string directory = Path.Combine(corpus.WorkDirectory, BackendId);
        if (Directory.Exists(directory)) Directory.Delete(directory, recursive: true);
        Directory.CreateDirectory(directory);

        _corpus = corpus;
        _blocks.Clear();
        _filters.Clear();
        _fileBlocks.Clear();
        _active.Clear();

        _store = new ContentBlockStore(Path.Combine(directory, "blocks"));
        _filterStream = new FileStream(
            Path.Combine(directory, "filters.bin"),
            FileMode.Create,
            FileAccess.ReadWrite,
            FileShare.Read,
            64 * 1024,
            FileOptions.Asynchronous | FileOptions.SequentialScan);

        foreach (ContentDocument document in corpus.Documents)
        {
            cancellationToken.ThrowIfCancellationRequested();
            await IndexDocumentAsync(document, cancellationToken).ConfigureAwait(false);
        }

        await _filterStream.FlushAsync(cancellationToken).ConfigureAwait(false);
        RefreshDiagnostics();
    }

    public async Task<IReadOnlyList<ContentMatch>> SearchAsync(ContentQuery query, CancellationToken cancellationToken)
    {
        ContentBlockStore store = _store ?? throw new InvalidOperationException("Backend is not built.");
        bool fallback = query.Mode != ContentQueryMode.Substring || ContentSemantics.RuneCount(query.Text) < 3;
        IReadOnlyList<ulong> grams = fallback ? Array.Empty<ulong>() : ContentSemantics.UniqueTrigrams(query.Text);

        StoredContentBlock[] candidates;
        lock (_gate)
        {
            IEnumerable<long> ids = _active;
            if (!fallback && grams.Count > 0)
            {
                ids = ids.Where(id =>
                    _filters.TryGetValue(id, out byte[]? filter) &&
                    MightContainAll(filter, grams));
            }
            else
            {
                fallback = true;
            }

            candidates = ids.Select(id => _blocks[id]).OrderBy(b => b.BlockId).ToArray();
        }

        var matches = new List<ContentMatch>();
        long bytes = 0;
        foreach (StoredContentBlock block in candidates)
        {
            cancellationToken.ThrowIfCancellationRequested();
            string text = await store.ReadTextAsync(block, cancellationToken).ConfigureAwait(false);
            bytes += block.StoreLength;
            matches.AddRange(ContentExactVerifier.Verify(block, text, query, BackendId, fallback));
        }

        _diagnostics.SearchCount++;
        _diagnostics.BytesRead += bytes;
        _diagnostics.CandidateBlocks += candidates.Length;
        _diagnostics.VerifiedBlocks += candidates.Length;
        if (fallback) _diagnostics.ScanFallbackCount++;
        return DeduplicateAndSort(matches);
    }

    public async Task ApplyChangesAsync(IReadOnlyList<ContentChange> changes, CancellationToken cancellationToken)
    {
        if (_store is null) throw new InvalidOperationException("Backend is not built.");

        foreach (ContentChange change in changes)
        {
            cancellationToken.ThrowIfCancellationRequested();
            DeactivateFile(change.FileKey);
            if (change.Kind != ContentChangeKind.Removed && change.Document is not null)
                await IndexDocumentAsync(change.Document, cancellationToken).ConfigureAwait(false);
        }

        _diagnostics.UpdateCount += changes.Count;
        RefreshDiagnostics();
    }

    public ContentBackendDiagnostics GetDiagnostics() => _diagnostics.Snapshot();

    public async ValueTask DisposeAsync() => await DisposeStorageAsync().ConfigureAwait(false);

    private async Task IndexDocumentAsync(ContentDocument document, CancellationToken cancellationToken)
    {
        ContentCorpus corpus = _corpus ?? throw new InvalidOperationException("Backend is not built.");
        ContentBlockStore store = _store ?? throw new InvalidOperationException("Backend is not built.");
        FileStream filterStream = _filterStream ?? throw new InvalidOperationException("Backend is not built.");

        IReadOnlyList<StoredContentBlock> blocks = await store.AppendDocumentAsync(
            document, corpus.BlockSizeChars, corpus.OverlapChars, cancellationToken).ConfigureAwait(false);

        var ids = new List<long>(blocks.Count);
        foreach (StoredContentBlock block in blocks)
        {
            string text = await store.ReadTextAsync(block, cancellationToken).ConfigureAwait(false);
            byte[] filter = CreateFilter(ContentSemantics.UniqueTrigrams(text));
            await filterStream.WriteAsync(filter.AsMemory(), cancellationToken).ConfigureAwait(false);

            lock (_gate)
            {
                _blocks[block.BlockId] = block;
                _filters[block.BlockId] = filter;
                _active.Add(block.BlockId);
            }
            ids.Add(block.BlockId);
        }

        lock (_gate) _fileBlocks[document.FileKey] = ids;
    }

    private void DeactivateFile(ContentFileKey fileKey)
    {
        lock (_gate)
        {
            if (!_fileBlocks.TryGetValue(fileKey, out List<long>? ids)) return;
            foreach (long id in ids) _active.Remove(id);
            _fileBlocks.Remove(fileKey);
        }
    }

    private static byte[] CreateFilter(IReadOnlyList<ulong> grams)
    {
        var filter = new byte[FilterBytes];
        foreach (ulong gram in grams)
        {
            ulong h1 = Mix(gram);
            ulong h2 = Mix(gram ^ 0x9E3779B97F4A7C15UL) | 1UL;
            for (int i = 0; i < 5; i++)
            {
                int bit = (int)((h1 + (ulong)i * h2) % (FilterBytes * 8UL));
                filter[bit >> 3] |= (byte)(1 << (bit & 7));
            }
        }
        return filter;
    }

    private static bool MightContainAll(byte[] filter, IReadOnlyList<ulong> grams)
    {
        foreach (ulong gram in grams)
        {
            ulong h1 = Mix(gram);
            ulong h2 = Mix(gram ^ 0x9E3779B97F4A7C15UL) | 1UL;
            for (int i = 0; i < 5; i++)
            {
                int bit = (int)((h1 + (ulong)i * h2) % (FilterBytes * 8UL));
                if ((filter[bit >> 3] & (1 << (bit & 7))) == 0) return false;
            }
        }
        return true;
    }

    private static ulong Mix(ulong value)
    {
        value ^= value >> 30;
        value *= 0xBF58476D1CE4E5B9UL;
        value ^= value >> 27;
        value *= 0x94D049BB133111EBUL;
        value ^= value >> 31;
        return value;
    }

    private void RefreshDiagnostics()
    {
        _diagnostics.IndexedBlocks = _blocks.Count;
        _diagnostics.ActiveBlocks = _active.Count;
        _diagnostics.PersistentBytes = (_store?.PersistentBytes ?? 0) + (_filterStream?.Length ?? 0);
    }

    private async Task DisposeStorageAsync()
    {
        if (_filterStream is not null)
        {
            await _filterStream.FlushAsync().ConfigureAwait(false);
            await _filterStream.DisposeAsync().ConfigureAwait(false);
            _filterStream = null;
        }
        if (_store is not null)
        {
            await _store.DisposeAsync().ConfigureAwait(false);
            _store = null;
        }
    }

    private static IReadOnlyList<ContentMatch> DeduplicateAndSort(IEnumerable<ContentMatch> matches) =>
        matches
            .GroupBy(m => (m.FileKey, m.DecodedCharOffset, m.MatchLength))
            .Select(g => g.First())
            .OrderBy(m => m.ExactPath, StringComparer.Ordinal)
            .ThenBy(m => m.DecodedCharOffset)
            .ToArray();
}

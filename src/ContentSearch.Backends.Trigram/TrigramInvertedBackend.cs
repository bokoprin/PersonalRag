using PersonalRag.ContentSearch.Core;

namespace PersonalRag.ContentSearch.Backends.Trigram;

public sealed class TrigramInvertedBackend : IContentSearchBackend
{
    private const double HighFrequencyRatio = 0.20;

    private readonly ContentBackendDiagnostics _diagnostics = new();
    private readonly object _gate = new();
    private readonly Dictionary<long, StoredContentBlock> _blocks = new();
    private readonly Dictionary<ContentFileKey, List<long>> _fileBlocks = new();
    private readonly HashSet<long> _active = new();
    private readonly Dictionary<ulong, PostingLocation> _baseDirectory = new();
    private readonly HashSet<ulong> _omittedBaseKeys = new();
    private readonly Dictionary<ulong, List<long>> _deltaPostings = new();

    private ContentCorpus? _corpus;
    private ContentBlockStore? _store;
    private FileStream? _postingsStream;

    public string BackendId => "trigram";

    public async Task BuildAsync(ContentCorpus corpus, CancellationToken cancellationToken)
    {
        await DisposeStorageAsync().ConfigureAwait(false);

        string directory = Path.Combine(corpus.WorkDirectory, BackendId);
        if (Directory.Exists(directory)) Directory.Delete(directory, recursive: true);
        Directory.CreateDirectory(directory);

        _corpus = corpus;
        _blocks.Clear();
        _fileBlocks.Clear();
        _active.Clear();
        _baseDirectory.Clear();
        _omittedBaseKeys.Clear();
        _deltaPostings.Clear();

        _store = new ContentBlockStore(Path.Combine(directory, "blocks"));

        var buildPostings = new Dictionary<ulong, List<long>>();
        foreach (ContentDocument document in corpus.Documents)
        {
            cancellationToken.ThrowIfCancellationRequested();
            IReadOnlyList<StoredContentBlock> blocks = await _store.AppendDocumentAsync(
                document, corpus.BlockSizeChars, corpus.OverlapChars, cancellationToken).ConfigureAwait(false);

            var ids = new List<long>(blocks.Count);
            foreach (StoredContentBlock block in blocks)
            {
                string text = await _store.ReadTextAsync(block, cancellationToken).ConfigureAwait(false);
                foreach (ulong gram in ContentSemantics.UniqueTrigrams(text))
                {
                    if (!buildPostings.TryGetValue(gram, out List<long>? list))
                    {
                        list = new List<long>();
                        buildPostings.Add(gram, list);
                    }
                    list.Add(block.BlockId);
                }

                _blocks[block.BlockId] = block;
                _active.Add(block.BlockId);
                ids.Add(block.BlockId);
            }
            _fileBlocks[document.FileKey] = ids;
        }

        _postingsStream = new FileStream(
            Path.Combine(directory, "postings.bin"), FileMode.Create, FileAccess.ReadWrite, FileShare.Read,
            64 * 1024, FileOptions.Asynchronous | FileOptions.RandomAccess);

        int totalBlocks = Math.Max(1, _active.Count);
        foreach ((ulong key, List<long> raw) in buildPostings.OrderBy(kv => kv.Key))
        {
            cancellationToken.ThrowIfCancellationRequested();
            raw.Sort();
            if ((double)raw.Count / totalBlocks > HighFrequencyRatio)
            {
                _omittedBaseKeys.Add(key);
                continue;
            }

            long offset = _postingsStream.Position;
            byte[] encoded = EncodeDeltaVarints(raw);
            await _postingsStream.WriteAsync(encoded.AsMemory(), cancellationToken).ConfigureAwait(false);
            _baseDirectory[key] = new PostingLocation(offset, encoded.Length, raw.Count);
        }

        await _postingsStream.FlushAsync(cancellationToken).ConfigureAwait(false);
        RefreshDiagnostics();
    }

    public async Task<IReadOnlyList<ContentMatch>> SearchAsync(ContentQuery query, CancellationToken cancellationToken)
    {
        ContentBlockStore store = _store ?? throw new InvalidOperationException("Backend is not built.");

        bool fallback = query.Mode != ContentQueryMode.Substring || ContentSemantics.RuneCount(query.Text) < 3;
        IReadOnlyList<ulong> grams = fallback ? Array.Empty<ulong>() : ContentSemantics.UniqueTrigrams(query.Text);

        long[] candidateIds;
        if (fallback || grams.Count == 0)
        {
            fallback = true;
            lock (_gate) candidateIds = _active.OrderBy(id => id).ToArray();
        }
        else
        {
            candidateIds = await BuildCandidatesAsync(grams, cancellationToken).ConfigureAwait(false);
            if (candidateIds.Length == 0 && !HasAnyIndexedGram(grams))
            {
                fallback = true;
                lock (_gate) candidateIds = _active.OrderBy(id => id).ToArray();
            }
        }

        StoredContentBlock[] candidates;
        lock (_gate)
        {
            candidates = candidateIds
                .Where(id => _active.Contains(id) && _blocks.ContainsKey(id))
                .Distinct()
                .Select(id => _blocks[id])
                .OrderBy(b => b.BlockId)
                .ToArray();
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
        ContentCorpus corpus = _corpus ?? throw new InvalidOperationException("Backend is not built.");
        ContentBlockStore store = _store ?? throw new InvalidOperationException("Backend is not built.");

        foreach (ContentChange change in changes)
        {
            cancellationToken.ThrowIfCancellationRequested();
            DeactivateFile(change.FileKey);
            if (change.Kind == ContentChangeKind.Removed || change.Document is null) continue;

            IReadOnlyList<StoredContentBlock> blocks = await store.AppendDocumentAsync(
                change.Document, corpus.BlockSizeChars, corpus.OverlapChars, cancellationToken).ConfigureAwait(false);

            var ids = new List<long>(blocks.Count);
            foreach (StoredContentBlock block in blocks)
            {
                string text = await store.ReadTextAsync(block, cancellationToken).ConfigureAwait(false);
                foreach (ulong gram in ContentSemantics.UniqueTrigrams(text))
                {
                    if (!_deltaPostings.TryGetValue(gram, out List<long>? list))
                    {
                        list = new List<long>();
                        _deltaPostings.Add(gram, list);
                    }
                    list.Add(block.BlockId);
                }

                lock (_gate)
                {
                    _blocks[block.BlockId] = block;
                    _active.Add(block.BlockId);
                }
                ids.Add(block.BlockId);
            }

            lock (_gate) _fileBlocks[change.FileKey] = ids;
        }

        _diagnostics.UpdateCount += changes.Count;
        RefreshDiagnostics();
    }

    public ContentBackendDiagnostics GetDiagnostics() => _diagnostics.Snapshot();

    public async ValueTask DisposeAsync() => await DisposeStorageAsync().ConfigureAwait(false);

    private async Task<long[]> BuildCandidatesAsync(IReadOnlyList<ulong> grams, CancellationToken cancellationToken)
    {
        var available = new List<(ulong Key, int Estimate)>();
        foreach (ulong gram in grams)
        {
            if (_omittedBaseKeys.Contains(gram)) continue;

            int estimate = 0;
            if (_baseDirectory.TryGetValue(gram, out PostingLocation location)) estimate += location.Count;
            if (_deltaPostings.TryGetValue(gram, out List<long>? delta)) estimate += delta.Count;
            if (estimate > 0) available.Add((gram, estimate));
        }

        if (available.Count == 0) return Array.Empty<long>();
        available.Sort((a, b) => a.Estimate.CompareTo(b.Estimate));

        HashSet<long>? intersection = null;
        foreach ((ulong key, _) in available)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var current = new HashSet<long>();

            if (_baseDirectory.TryGetValue(key, out PostingLocation location))
            {
                foreach (long id in await ReadPostingAsync(location, cancellationToken).ConfigureAwait(false))
                    current.Add(id);
            }
            if (_deltaPostings.TryGetValue(key, out List<long>? delta))
            {
                foreach (long id in delta) current.Add(id);
            }

            if (intersection is null) intersection = current;
            else intersection.IntersectWith(current);
            if (intersection.Count == 0) break;
        }

        return intersection?.OrderBy(id => id).ToArray() ?? Array.Empty<long>();
    }

    private bool HasAnyIndexedGram(IReadOnlyList<ulong> grams)
    {
        foreach (ulong gram in grams)
        {
            if (_omittedBaseKeys.Contains(gram)) continue;
            if (_baseDirectory.ContainsKey(gram) || _deltaPostings.ContainsKey(gram)) return true;
        }
        return false;
    }

    private async Task<IReadOnlyList<long>> ReadPostingAsync(PostingLocation location, CancellationToken cancellationToken)
    {
        FileStream stream = _postingsStream ?? throw new InvalidOperationException("Backend is not built.");
        byte[] bytes = new byte[location.Length];
        int total = 0;
        while (total < bytes.Length)
        {
            int read = await RandomAccess.ReadAsync(
                stream.SafeFileHandle, bytes.AsMemory(total), location.Offset + total, cancellationToken).ConfigureAwait(false);
            if (read == 0) throw new EndOfStreamException("Posting blob is truncated.");
            total += read;
        }
        return DecodeDeltaVarints(bytes, location.Count);
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

    private static byte[] EncodeDeltaVarints(IReadOnlyList<long> ids)
    {
        using var stream = new MemoryStream();
        long previous = 0;
        foreach (long id in ids)
        {
            ulong delta = checked((ulong)(id - previous));
            WriteVarint(stream, delta);
            previous = id;
        }
        return stream.ToArray();
    }

    private static IReadOnlyList<long> DecodeDeltaVarints(byte[] bytes, int count)
    {
        var result = new long[count];
        int offset = 0;
        long current = 0;
        for (int i = 0; i < count; i++)
        {
            ulong delta = ReadVarint(bytes, ref offset);
            current = checked(current + (long)delta);
            result[i] = current;
        }
        return result;
    }

    private static void WriteVarint(Stream stream, ulong value)
    {
        while (value >= 0x80)
        {
            stream.WriteByte((byte)(value | 0x80));
            value >>= 7;
        }
        stream.WriteByte((byte)value);
    }

    private static ulong ReadVarint(byte[] bytes, ref int offset)
    {
        ulong value = 0;
        int shift = 0;
        while (true)
        {
            if (offset >= bytes.Length || shift > 63) throw new InvalidDataException("Invalid posting varint.");
            byte b = bytes[offset++];
            value |= (ulong)(b & 0x7F) << shift;
            if ((b & 0x80) == 0) return value;
            shift += 7;
        }
    }

    private void RefreshDiagnostics()
    {
        _diagnostics.IndexedBlocks = _blocks.Count;
        _diagnostics.ActiveBlocks = _active.Count;
        _diagnostics.PersistentBytes = (_store?.PersistentBytes ?? 0) + (_postingsStream?.Length ?? 0);
    }

    private async Task DisposeStorageAsync()
    {
        if (_postingsStream is not null)
        {
            await _postingsStream.FlushAsync().ConfigureAwait(false);
            await _postingsStream.DisposeAsync().ConfigureAwait(false);
            _postingsStream = null;
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

    private readonly record struct PostingLocation(long Offset, int Length, int Count);
}

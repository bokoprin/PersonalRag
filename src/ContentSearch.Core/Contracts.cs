namespace PersonalRag.ContentSearch.Core;

public readonly record struct ContentFileKey(string Value)
{
    public override string ToString() => Value;
}

public enum ContentQueryMode
{
    Substring,
    Regex
}

public enum ContentIndexStatus
{
    Indexed,
    Unsupported,
    Binary,
    DecodeFailed,
    TooLarge
}

public enum ContentChangeKind
{
    Added,
    Updated,
    Removed,
    Renamed,
    Moved
}

public sealed record ContentDocument(
    ContentFileKey FileKey,
    string ExactPath,
    long SizeBytes,
    DateTime ModifiedUtc);

public sealed record ContentCorpus(
    IReadOnlyList<ContentDocument> Documents,
    string WorkDirectory,
    int BlockSizeChars = 65_536,
    int OverlapChars = 256);

public sealed record ContentQuery(
    string Text,
    ContentQueryMode Mode = ContentQueryMode.Substring,
    bool CaseSensitive = false);

public sealed record ContentMatch(
    ContentFileKey FileKey,
    string ExactPath,
    long DecodedCharOffset,
    int Line,
    int Column,
    int MatchLength,
    string Snippet,
    long BlockId,
    string BackendId,
    bool UsedScanFallback);

public sealed record ContentChange(
    ContentChangeKind Kind,
    ContentDocument? Document,
    ContentFileKey FileKey,
    string? OldPath = null);

public sealed class ContentBackendDiagnostics
{
    public long SearchCount { get; set; }
    public long BytesRead { get; set; }
    public long CandidateBlocks { get; set; }
    public long VerifiedBlocks { get; set; }
    public long ScanFallbackCount { get; set; }
    public long IndexedBlocks { get; set; }
    public long ActiveBlocks { get; set; }
    public long PersistentBytes { get; set; }
    public long UpdateCount { get; set; }

    public ContentBackendDiagnostics Snapshot() => new()
    {
        SearchCount = SearchCount,
        BytesRead = BytesRead,
        CandidateBlocks = CandidateBlocks,
        VerifiedBlocks = VerifiedBlocks,
        ScanFallbackCount = ScanFallbackCount,
        IndexedBlocks = IndexedBlocks,
        ActiveBlocks = ActiveBlocks,
        PersistentBytes = PersistentBytes,
        UpdateCount = UpdateCount
    };
}

public interface IContentSearchBackend : IAsyncDisposable
{
    string BackendId { get; }

    Task BuildAsync(ContentCorpus corpus, CancellationToken cancellationToken);

    Task<IReadOnlyList<ContentMatch>> SearchAsync(ContentQuery query, CancellationToken cancellationToken);

    Task ApplyChangesAsync(IReadOnlyList<ContentChange> changes, CancellationToken cancellationToken);

    ContentBackendDiagnostics GetDiagnostics();
}

public sealed record ExtractedTextInfo(ContentIndexStatus Status, string EncodingName, string? Error = null);

public sealed record ExtractedBlock(
    long BlockId,
    ContentFileKey FileKey,
    string ExactPath,
    int BlockOrdinal,
    long DecodedCharStart,
    int BaseLine,
    string Text);

public sealed record StoredContentBlock(
    long BlockId,
    ContentFileKey FileKey,
    string ExactPath,
    int BlockOrdinal,
    long DecodedCharStart,
    int BaseLine,
    long StoreOffset,
    int StoreLength);

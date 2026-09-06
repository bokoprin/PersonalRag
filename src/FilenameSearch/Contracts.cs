using System.Collections.ObjectModel;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Stable filesystem identity. On Windows/NTFS VolumeId is the volume GUID and NativeId
/// is the file reference number. IsNative=false is a deterministic fallback only.
/// </summary>
public readonly record struct FileKey(string VolumeId, ulong NativeId, bool IsNative)
{
    public static FileKey Synthetic(string volumeId, ulong id) => new(volumeId, id, false);
    public override string ToString() => $"{VolumeId}:{NativeId:x16}:{(IsNative ? "n" : "s")}";
}

public enum SearchScope { Filename, FullPath }

public sealed record SearchRequest(
    string Query,
    SearchScope Scope = SearchScope.Filename,
    bool CaseSensitive = false,
    int Limit = 100,
    long RequestId = 0)
{
    public SearchRequest Validate()
    {
        if (Query is null) throw new ArgumentNullException(nameof(Query));
        if (Limit is < 0 or > 100_000) throw new ArgumentOutOfRangeException(nameof(Limit));
        return this;
    }
}

/// <summary>
/// Exact filesystem metadata exposed to callers. Name and FullPath preserve the original
/// filesystem spelling; search normalization is an internal concern and must not rewrite them.
/// </summary>
public sealed record FilenameRecord(
    int FileId,
    int? ParentId,
    string Name,
    string FullPath,
    ulong SizeBytes,
    DateTime ModifiedUtc,
    byte Flags = 1)
{
    public FileKey Key { get; init; } = FileKey.Synthetic("legacy", unchecked((ulong)FileId));
    public FileKey? ParentKey { get; init; }
    public bool IsDirectory => (Flags & 2) != 0;
}

public sealed record FilenameSearchResult(
    IReadOnlyList<FilenameRecord> Records,
    double ElapsedMs,
    int Candidates,
    bool UsedScan,
    long RequestId = 0)
{
    public static FilenameSearchResult Empty(long requestId = 0) =>
        new(new ReadOnlyCollection<FilenameRecord>([]), 0, 0, false, requestId);
}

public enum CatalogChangeKind
{
    Added,
    Updated,
    Removed,
    Renamed,
    Moved,
    Reconciled
}

public sealed record CatalogChange(
    CatalogChangeKind Kind,
    FileKey Key,
    FilenameRecord? Before,
    FilenameRecord? After);

public sealed record CatalogChangeBatch(
    long Generation,
    IReadOnlyList<CatalogChange> Changes,
    bool Reconciled = false,
    string? SourceId = null,
    long SourceGeneration = 0);

public sealed record CatalogSnapshot(
    long Generation,
    IReadOnlyList<FilenameRecord> Records,
    IReadOnlyDictionary<string, long> SourceGenerations);

/// <summary>
/// Shared catalog boundary used by the GUI and by the future Content Engine.
/// Content indexing subscribes to typed changes instead of creating a second filesystem watcher.
/// </summary>
public interface IFilenameCatalog : IAsyncDisposable
{
    string Status { get; }
    long Generation { get; }
    int RecordCount { get; }
    Task Ready { get; }
    event Action<CatalogChangeBatch>? Changed;
    CatalogSnapshot GetSnapshot();
    FilenameSearchResult Search(SearchRequest request);
    Task WaitForIdleAsync(TimeSpan timeout, CancellationToken cancellationToken = default);
}

public interface IFilenameSearch : IDisposable
{
    IReadOnlyList<FilenameRecord> Records { get; }
    int OverlayCount { get; }
    bool ShouldCompact { get; }
    void Build(IReadOnlyList<FilenameRecord> records);
    void LoadBase(string indexPath, IReadOnlyList<FilenameRecord> exactRecords);
    void SaveBase(string indexPath);
    FilenameSearchResult Search(SearchRequest request);
    void Upsert(FilenameRecord record);
    bool Remove(int fileId);
}

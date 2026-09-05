using System.Collections.ObjectModel;

namespace PersonalRag.FilenameSearch;

/// <summary>The string field to search.</summary>
public enum SearchScope
{
    Filename,
    FullPath
}

/// <summary>Stable request boundary shared by the GUI and future search engines.</summary>
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

/// <summary>Canonical metadata exposed to callers. The original Unicode strings are retained.</summary>
public sealed record FilenameRecord(
    int FileId,
    int? ParentId,
    string Name,
    string FullPath,
    ulong SizeBytes,
    DateTime ModifiedUtc,
    byte Flags = 1)
{
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

/// <summary>Product-level search boundary. It intentionally contains no document-content operations.</summary>
public interface IFilenameSearch : IDisposable
{
    IReadOnlyList<FilenameRecord> Records { get; }
    void Build(IReadOnlyList<FilenameRecord> records);
    void Load(string store);
    void SaveAtomic(string store);
    FilenameSearchResult Search(SearchRequest request);
    void Upsert(FilenameRecord record);
    bool Remove(int fileId);
}

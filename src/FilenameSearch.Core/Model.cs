using System.Text;

namespace FilenameSearch.Core;

public enum FilenameScope { Filename, FullPath }

public sealed record FileRecord(
    int FileId,
    int? ParentId,
    string Name,
    string FullPath,
    ulong SizeBytes,
    long ModifiedUtcTicks,
    byte Flags = 1);

public sealed record FilenameQuery(
    string Query,
    FilenameScope Scope = FilenameScope.Filename,
    bool CaseSensitive = false,
    int Limit = 100);

public sealed record SearchResult(
    IReadOnlyList<FileRecord> Records,
    double ElapsedMs,
    int Candidates,
    bool UsedScan);

public interface IFilenameSearchEngine
{
    string RouteName { get; }
    IReadOnlyList<FileRecord> Records { get; }
    void Build(IReadOnlyList<FileRecord> records);
    void Load(string store);
    void Save(string store);
    SearchResult Search(FilenameQuery request);
    void Upsert(FileRecord record);
    bool Remove(int fileId);
}

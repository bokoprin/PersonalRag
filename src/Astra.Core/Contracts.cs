namespace Astra.Core;

public enum ContentMode { Literal, Regex, Wildcard }
public enum FileScope { Filename, FullPath }
public record SearchRequest(string FileQuery = "", string ContentQuery = "", FileScope Scope = FileScope.Filename,
    ContentMode Mode = ContentMode.Literal, bool CaseSensitive = false);
public record TextUnit(string Location, string Text);
public record Hit(long Ordinal, string Location, string Text, int Start, int Length);
public record FileEntry(string Path, long Size, long ModifiedUtcTicks, string? Unsearchable,
    [property: System.Text.Json.Serialization.JsonIgnore] ReadOnlyMemory<byte> Signature)
{
    public string Name { get; } = System.IO.Path.GetFileName(Path);
}
public record SearchRow(FileEntry File, long Hits, bool HitsComplete, Hit? FirstHit);
public record SearchPage(IReadOnlyList<SearchRow> Rows, int NextOffset, bool Complete, double ElapsedMs,
    IReadOnlyList<string> Warnings);
public record HitPage(IReadOnlyList<Hit> Hits, long Total, bool Complete);
public record IndexSnapshot(string Root, FileEntry[] Files, DateTime CreatedUtc);
public interface ITextExtractor
{
    IEnumerable<TextUnit> Extract(string path, CancellationToken cancellationToken = default);
}

public interface IDeterministicSearch
{
    SearchPage Search(SearchRequest request, int offset = 0, int limit = 100, CancellationToken cancellationToken = default);
    HitPage GetHits(string path, SearchRequest request, long offset = 0, int limit = 32, bool countAll = false,
        CancellationToken cancellationToken = default);
}

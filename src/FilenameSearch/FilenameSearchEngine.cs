using CoreFileRecord = FilenameSearch.Core.FileRecord;
using CoreQuery = FilenameSearch.Core.FilenameQuery;
using CoreScope = FilenameSearch.Core.FilenameScope;
using RouteCEngine = FilenameSearch.RouteC.RouteCEngine;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Product adapter for the selected Route C engine. Route A and Route B are intentionally
/// not referenced by this project.
/// </summary>
public sealed class FilenameSearchEngine : IFilenameSearch
{
    private readonly object gate = new();
    private RouteCEngine inner = new();
    private bool disposed;

    public IReadOnlyList<FilenameRecord> Records
    {
        get
        {
            lock (gate)
            {
                ThrowIfDisposed();
                return inner.Records.Select(FromCore).ToArray();
            }
        }
    }

    public void Build(IReadOnlyList<FilenameRecord> records)
    {
        ArgumentNullException.ThrowIfNull(records);
        lock (gate)
        {
            ThrowIfDisposed();
            inner.Build(records.Select(ToCore).ToArray());
        }
    }

    public void Load(string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        lock (gate)
        {
            ThrowIfDisposed();
            inner.Load(Path.GetFullPath(store));
        }
    }

    /// <summary>Preloads lazy Route C filename data so the first interactive query is disk independent.</summary>
    public void WarmUp()
    {
        lock (gate)
        {
            ThrowIfDisposed();
            inner.WarmUp();
        }
    }

    /// <summary>Writes a temporary store, then atomically replaces the visible store.</summary>
    public void SaveAtomic(string store)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        string destination = Path.GetFullPath(store);
        string? parent = Path.GetDirectoryName(destination);
        if (parent is null) throw new ArgumentException("Store path has no parent directory", nameof(store));
        Directory.CreateDirectory(parent);
        string temp = destination + "." + Guid.NewGuid().ToString("N") + ".tmp";
        lock (gate)
        {
            ThrowIfDisposed();
            try
            {
                inner.Save(temp);
                // A loaded Route C instance keeps a read handle for lazy tables. Close it
                // before replacing the file, then reload the committed file below.
                inner.Dispose();
                if (File.Exists(destination)) File.Replace(temp, destination, null, ignoreMetadataErrors: true);
                else File.Move(temp, destination);
                inner.Load(destination);
            }
            finally
            {
                TryDelete(temp);
            }
        }
    }

    /// <summary>Persists an immutable metadata snapshot without taking the live engine lock.</summary>
    public static void SaveSnapshotAtomic(string store, IReadOnlyList<FilenameRecord> records)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(store);
        ArgumentNullException.ThrowIfNull(records);
        using var snapshot = new FilenameSearchEngine();
        snapshot.Build(records);
        snapshot.SaveAtomic(store);
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        request.Validate();
        lock (gate)
        {
            ThrowIfDisposed();
            var result = inner.Search(new CoreQuery(
                request.Query,
                request.Scope == SearchScope.Filename ? CoreScope.Filename : CoreScope.FullPath,
                request.CaseSensitive,
                request.Limit));
            return new FilenameSearchResult(result.Records.Select(FromCore).ToArray(), result.ElapsedMs,
                result.Candidates, result.UsedScan, request.RequestId);
        }
    }

    public void Upsert(FilenameRecord record)
    {
        ArgumentNullException.ThrowIfNull(record);
        lock (gate)
        {
            ThrowIfDisposed();
            inner.Upsert(ToCore(record));
        }
    }

    public bool Remove(int fileId)
    {
        lock (gate)
        {
            ThrowIfDisposed();
            return inner.Remove(fileId);
        }
    }

    public void Dispose()
    {
        lock (gate)
        {
            if (disposed) return;
            inner.Dispose();
            disposed = true;
        }
        GC.SuppressFinalize(this);
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(disposed, this);

    private static CoreFileRecord ToCore(FilenameRecord record) => new(
        record.FileId, record.ParentId, record.Name, record.FullPath, record.SizeBytes,
        record.ModifiedUtc.Ticks, record.Flags);

    private static FilenameRecord FromCore(CoreFileRecord record) => new(
        record.FileId, record.ParentId, record.Name, record.FullPath, record.SizeBytes,
        new DateTime(record.ModifiedUtcTicks, DateTimeKind.Utc), record.Flags);

    private static void TryDelete(string path)
    {
        try { if (File.Exists(path)) File.Delete(path); }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
    }
}

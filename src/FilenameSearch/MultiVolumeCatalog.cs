using System.Collections.Concurrent;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Federates all fixed local volumes while keeping each volume's identity, persistence generation,
/// watcher and Route C base independent. FileKey prevents cross-volume identity collisions.
/// </summary>
public sealed class MultiVolumeCatalog : IFilenameCatalog
{
    private readonly FileSystemCatalog[] catalogs;
    private readonly string[] openWarnings;
    private long generation;
    private bool disposed;

    private MultiVolumeCatalog(FileSystemCatalog[] catalogs, string[] openWarnings)
    {
        this.catalogs = catalogs;
        this.openWarnings = openWarnings;
        generation = catalogs.Length == 0 ? 0 : 1;
        foreach (FileSystemCatalog catalog in catalogs) catalog.Changed += ForwardChanged;
        Ready = Task.WhenAll(catalogs.Select(c => c.Ready));
    }

    public string Status
    {
        get
        {
            if (openWarnings.Length > 0)
                return $"{ReadyStatus()} · {openWarnings.Length} volume warning(s)";
            return ReadyStatus();
        }
    }

    public long Generation => Interlocked.Read(ref generation);
    public int RecordCount => catalogs.Sum(c => c.RecordCount);
    public Task Ready { get; }
    public event Action<CatalogChangeBatch>? Changed;

    public IReadOnlyList<string> Roots => catalogs.Select(c => c.Root).ToArray();
    public IReadOnlyList<string> OpenWarnings => openWarnings;

    public static async Task<MultiVolumeCatalog> OpenLocalFixedVolumesAsync(
        string storeRoot,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(storeRoot);
        Directory.CreateDirectory(storeRoot);
        IReadOnlyList<VolumeDescriptor> volumes = LocalVolumeDiscovery.FixedReadyVolumes();
        if (volumes.Count == 0) throw new InvalidOperationException("No fixed local volume is ready");

        var opened = new ConcurrentBag<FileSystemCatalog>();
        var warnings = new ConcurrentBag<string>();
        await Task.WhenAll(volumes.Select(volume => Task.Run(() =>
        {
            cancellationToken.ThrowIfCancellationRequested();
            try
            {
                string key = SafeStoreKey(volume.VolumeId);
                string store = Path.Combine(storeRoot, key, "index.manifest");
                opened.Add(FileSystemCatalog.Open(volume.Root, store, [Path.GetFullPath(storeRoot)]));
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException)
            {
                warnings.Add($"{volume.Root}: {ex.Message}");
            }
        }, cancellationToken))).ConfigureAwait(false);

        FileSystemCatalog[] catalogs = opened.OrderBy(c => c.Root, StringComparer.OrdinalIgnoreCase).ToArray();
        if (catalogs.Length == 0)
            throw new InvalidOperationException("No fixed local volume could be opened: " + string.Join("; ", warnings));
        return new MultiVolumeCatalog(catalogs, warnings.OrderBy(x => x, StringComparer.Ordinal).ToArray());
    }

    public CatalogSnapshot GetSnapshot()
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        var records = new List<FilenameRecord>();
        var sources = new Dictionary<string, long>(StringComparer.Ordinal);
        foreach (FileSystemCatalog catalog in catalogs)
        {
            CatalogSnapshot snapshot = catalog.GetSnapshot();
            records.AddRange(snapshot.Records);
            foreach ((string source, long sourceGeneration) in snapshot.SourceGenerations)
                sources[source] = sourceGeneration;
        }
        return new CatalogSnapshot(Generation, records, sources);
    }

    public FilenameSearchResult Search(SearchRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        request.Validate();
        ObjectDisposedException.ThrowIf(disposed, this);
        var watch = Stopwatch.StartNew();
        var results = new ConcurrentBag<FilenameSearchResult>();

        Parallel.ForEach(catalogs, catalog =>
        {
            var localRequest = request with { RequestId = request.RequestId };
            results.Add(catalog.Search(localRequest));
        });

        var dedup = new Dictionary<(FileKey Key, string Path), FilenameRecord>();
        int candidates = 0;
        bool usedScan = false;
        foreach (FilenameSearchResult result in results)
        {
            candidates += result.Candidates;
            usedScan |= result.UsedScan;
            foreach (FilenameRecord record in result.Records)
                dedup[(record.Key, record.FullPath)] = record;
        }

        FilenameRecord[] ordered = dedup.Values
            .OrderBy(r => r.FullPath, StringComparer.OrdinalIgnoreCase)
            .ThenBy(r => r.FullPath, StringComparer.Ordinal)
            .ToArray();
        if (request.Limit > 0 && ordered.Length > request.Limit)
            ordered = ordered[..request.Limit];

        watch.Stop();
        return new FilenameSearchResult(
            ordered,
            watch.Elapsed.TotalMilliseconds,
            candidates,
            usedScan,
            request.RequestId);
    }

    public async Task WaitForIdleAsync(TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        using var timeoutCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeoutCts.CancelAfter(timeout);
        await Task.WhenAll(catalogs.Select(c => c.WaitForIdleAsync(timeout, timeoutCts.Token))).ConfigureAwait(false);
    }

    private void ForwardChanged(CatalogChangeBatch source)
    {
        long next = Interlocked.Increment(ref generation);
        try { Changed?.Invoke(source with { Generation = next }); }
        catch { }
    }

    private string ReadyStatus()
    {
        if (catalogs.Any(c => c.Status != "Ready"))
            return string.Join(" / ", catalogs.Select(c => $"{Path.GetPathRoot(c.Root)} {c.Status}"));
        return $"Ready · {catalogs.Length} volume(s)";
    }

    private static string SafeStoreKey(string volumeId)
    {
        byte[] hash = SHA256.HashData(Encoding.UTF8.GetBytes(volumeId));
        return Convert.ToHexString(hash).ToLowerInvariant()[..20];
    }

    public async ValueTask DisposeAsync()
    {
        if (disposed) return;
        disposed = true;
        foreach (FileSystemCatalog catalog in catalogs) catalog.Changed -= ForwardChanged;
        foreach (FileSystemCatalog catalog in catalogs)
            await catalog.DisposeAsync().ConfigureAwait(false);
    }
}

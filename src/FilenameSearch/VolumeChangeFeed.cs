namespace PersonalRag.FilenameSearch;

/// <summary>
/// Volume-scoped change source boundary. The current production backend is the safe watcher
/// fallback. NTFS USN can replace it without changing FileSystemCatalog or Content Engine.
/// </summary>
internal interface IVolumeChangeFeed : IDisposable
{
    event Action<FileSystemEvent>? Changed;
    event Action? Overflow;
    void Start();
    void Stop();
}

internal sealed class WatcherVolumeChangeFeed : IVolumeChangeFeed
{
    private readonly FileSystemWatcher watcher;

    public WatcherVolumeChangeFeed(string root)
    {
        watcher = new FileSystemWatcher(root)
        {
            IncludeSubdirectories = true,
            InternalBufferSize = 64 * 1024,
            NotifyFilter = NotifyFilters.FileName | NotifyFilters.DirectoryName |
                           NotifyFilters.LastWrite | NotifyFilters.Size,
            Filter = "*"
        };
        watcher.Created += (_, args) => Changed?.Invoke(new FileSystemEvent(
            args.FullPath,
            Kind: FileSystemEventKind.Created,
            Reconcile: Directory.Exists(args.FullPath)));
        watcher.Changed += (_, args) => Changed?.Invoke(new FileSystemEvent(
            args.FullPath,
            Kind: FileSystemEventKind.Changed));
        watcher.Deleted += (_, args) => Changed?.Invoke(new FileSystemEvent(
            args.FullPath,
            Kind: FileSystemEventKind.Deleted));
        watcher.Renamed += (_, args) => Changed?.Invoke(new FileSystemEvent(
            args.FullPath,
            args.OldFullPath,
            FileSystemEventKind.Renamed,
            Directory.Exists(args.FullPath)));
        watcher.Error += (_, _) => Overflow?.Invoke();
    }

    public event Action<FileSystemEvent>? Changed;
    public event Action? Overflow;

    public void Start() => watcher.EnableRaisingEvents = true;
    public void Stop() => watcher.EnableRaisingEvents = false;
    public void Dispose() => watcher.Dispose();
}

internal static class VolumeChangeFeedFactory
{
    public static IVolumeChangeFeed Create(string root)
    {
        // USN/MFT direct enumeration is intentionally behind this boundary. Formal hardening
        // first measures all-volume startup/restart. If recursive fallback misses the gate,
        // Codex must activate an NTFS USN implementation here rather than touching catalog/search APIs.
        return new WatcherVolumeChangeFeed(root);
    }

    public static string NtfsUsnDecision(string root)
    {
        try
        {
            string? driveRoot = Path.GetPathRoot(Path.GetFullPath(root));
            if (!OperatingSystem.IsWindows() || driveRoot is null) return "fallback-watcher: non-Windows";
            var drive = new DriveInfo(driveRoot);
            if (!drive.IsReady || !drive.DriveFormat.Equals("NTFS", StringComparison.OrdinalIgnoreCase))
                return "fallback-watcher: non-NTFS";
            return "USN-capable boundary available; watcher fallback remains active until formal 1M/all-volume measurements justify direct journal access";
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return "fallback-watcher: " + ex.Message;
        }
    }
}

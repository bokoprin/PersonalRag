using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Microsoft.Win32.SafeHandles;

namespace PersonalRag.FilenameSearch;

/// <summary>
/// Volume-scoped change source boundary. The current production backend is the safe watcher
/// fallback. NTFS USN can replace it without changing FileSystemCatalog or Content Engine.
/// </summary>
internal interface IVolumeChangeFeed : IDisposable
{
    event Action<FileSystemEvent>? Changed;
    event Action? Overflow;
    bool FastCatchUpAvailable { get; }
    bool RequiresSnapshot { get; }
    void SetSnapshot(IReadOnlyList<FilenameRecord> records);
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
    public bool FastCatchUpAvailable => false;
    public bool RequiresSnapshot => false;

    public void SetSnapshot(IReadOnlyList<FilenameRecord> records) { }

    public void Start() => watcher.EnableRaisingEvents = true;
    public void Stop() => watcher.EnableRaisingEvents = false;
    public void Dispose() => watcher.Dispose();
}

/// <summary>
/// NTFS USN journal reader used only for stopped-app catch-up. FileSystemWatcher remains the
/// live event source boundary for the fallback implementation. The cursor is persisted only
/// on a clean stop, so a dirty shutdown replays from the last known safe point.
/// </summary>
internal sealed class UsnVolumeChangeFeed : IVolumeChangeFeed
{
    private const uint FsctlQueryUsnJournal = 0x000900F4;
    private const uint FsctlReadUsnJournal = 0x000900BB;
    private const uint ErrorHandleEof = 38;
    private const uint ErrorJournalEntryDeleted = 1178;
    private readonly string root;
    private readonly string cursorPath;
    private readonly string volumeRoot;
    private readonly FileSystemWatcher watcher;
    private readonly Dictionary<ulong, List<string>> pathsById = [];
    private readonly Dictionary<ulong, PendingRename> pendingRenames = [];
    private SafeFileHandle? volume;
    private JournalCursor? cursorAtStart;
    private ulong journalIdAtStart;
    private long nextUsnAtStart;
    private bool started;
    private bool stopped;
    private bool fastCatchUpAvailable = true;

    public UsnVolumeChangeFeed(string root, string cursorPath)
    {
        this.root = Path.GetFullPath(root);
        this.cursorPath = Path.GetFullPath(cursorPath);
        volumeRoot = Path.GetPathRoot(this.root) ?? throw new ArgumentException("Root has no volume", nameof(root));
        watcher = new FileSystemWatcher(this.root)
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
    public bool FastCatchUpAvailable => fastCatchUpAvailable;
    public bool RequiresSnapshot => true;

    public void SetSnapshot(IReadOnlyList<FilenameRecord> records)
    {
        pathsById.Clear();
        foreach (FilenameRecord record in records)
        {
            if (!record.Key.IsNative || !PathIdentity.IsSameOrChild(root, record.FullPath)) continue;
            pathsById.TryGetValue(record.Key.NativeId, out List<string>? paths);
            (paths ??= []).Add(record.FullPath);
        }
    }

    public void Start()
    {
        if (started) return;
        started = true;
        // USN covers the interval while the process was stopped; the watcher
        // remains the live event source while this instance is running.
        watcher.EnableRaisingEvents = true;
        if (!TryOpenVolume(out SafeFileHandle? handle) || !TryQueryJournal(handle!, out JournalState state))
        {
            fastCatchUpAvailable = false;
            Overflow?.Invoke();
            return;
        }
        volume = handle;
        journalIdAtStart = state.JournalId;
        nextUsnAtStart = state.NextUsn;
        cursorAtStart = ReadCursor();
        if (cursorAtStart is null)
        {
            // A store created before this backend has no safe journal cursor. Request one
            // bounded reconcile before establishing the first cursor; otherwise a stopped
            // app could silently miss changes made before the backend was introduced.
            fastCatchUpAvailable = false;
            Overflow?.Invoke();
            return;
        }
        if (cursorAtStart.Value.JournalId != state.JournalId ||
            cursorAtStart.Value.NextUsn < state.FirstUsn || cursorAtStart.Value.NextUsn > state.NextUsn)
        {
            fastCatchUpAvailable = false;
            Overflow?.Invoke();
            return;
        }
        if (!ReadSince(cursorAtStart.Value.NextUsn, state))
        {
            fastCatchUpAvailable = false;
            Overflow?.Invoke();
        }
    }

    public void Stop()
    {
        if (stopped) return;
        stopped = true;
        watcher.EnableRaisingEvents = false;
        try
        {
            if (volume is not null && !volume.IsInvalid && TryQueryJournal(volume, out JournalState state))
                WriteCursor(new JournalCursor(state.JournalId, state.NextUsn));
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or JsonException) { }
        volume?.Dispose(); volume = null;
    }

    public void Dispose() { Stop(); watcher.Dispose(); }

    internal bool Probe()
    {
        if (!TryOpenVolume(out SafeFileHandle? handle) || handle is null) return false;
        try
        {
            if (!TryQueryJournal(handle, out JournalState state)) return false;
            return CanReadJournal(handle, state);
        }
        finally { handle.Dispose(); }
    }

    private static bool CanReadJournal(SafeFileHandle handle, JournalState state)
    {
        byte[] input = new byte[Marshal.SizeOf<ReadUsnJournalData>()];
        BinaryPrimitives.WriteInt64LittleEndian(input.AsSpan(0, 8), state.NextUsn);
        BinaryPrimitives.WriteUInt32LittleEndian(input.AsSpan(8, 4), 0xFFFFFFFF);
        BinaryPrimitives.WriteUInt32LittleEndian(input.AsSpan(12, 4), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(16, 8), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(24, 8), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(32, 8), state.JournalId);
        BinaryPrimitives.WriteUInt16LittleEndian(input.AsSpan(40, 2), 2);
        BinaryPrimitives.WriteUInt16LittleEndian(input.AsSpan(42, 2), 2);
        IntPtr inputPtr = Marshal.AllocHGlobal(input.Length);
        IntPtr outputPtr = Marshal.AllocHGlobal(1 << 16);
        try
        {
            Marshal.Copy(input, 0, inputPtr, input.Length);
            if (DeviceIoControl(handle, FsctlReadUsnJournal, inputPtr, input.Length, outputPtr, 1 << 16,
                    out _, IntPtr.Zero)) return true;
            return (uint)Marshal.GetLastWin32Error() == ErrorHandleEof;
        }
        finally { Marshal.FreeHGlobal(inputPtr); Marshal.FreeHGlobal(outputPtr); }
    }

    private bool ReadSince(long startUsn, JournalState state)
    {
        if (startUsn >= state.NextUsn) return true;
        byte[] input = new byte[Marshal.SizeOf<ReadUsnJournalData>()];
        BinaryPrimitives.WriteInt64LittleEndian(input.AsSpan(0, 8), startUsn);
        BinaryPrimitives.WriteUInt32LittleEndian(input.AsSpan(8, 4), 0xFFFFFFFF);
        BinaryPrimitives.WriteUInt32LittleEndian(input.AsSpan(12, 4), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(16, 8), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(24, 8), 0);
        BinaryPrimitives.WriteUInt64LittleEndian(input.AsSpan(32, 8), state.JournalId);
        BinaryPrimitives.WriteUInt16LittleEndian(input.AsSpan(40, 2), 2);
        BinaryPrimitives.WriteUInt16LittleEndian(input.AsSpan(42, 2), 2);
        IntPtr inputPtr = Marshal.AllocHGlobal(input.Length);
        IntPtr outputPtr = Marshal.AllocHGlobal(1 << 20);
        try
        {
            Marshal.Copy(input, 0, inputPtr, input.Length);
            long current = startUsn;
            while (current < state.NextUsn)
            {
                if (!DeviceIoControl(volume!, FsctlReadUsnJournal, inputPtr, input.Length, outputPtr, 1 << 20,
                        out uint returned, IntPtr.Zero))
                {
                    uint error = (uint)Marshal.GetLastWin32Error();
                    if (error == ErrorHandleEof) return true;
                    if (error == ErrorJournalEntryDeleted) return false;
                    return false;
                }
                if (returned < 8) return false;
                byte[] buffer = new byte[returned]; Marshal.Copy(outputPtr, buffer, 0, buffer.Length);
                long next = BinaryPrimitives.ReadInt64LittleEndian(buffer.AsSpan(0, 8));
                if (next <= current) return false;
                for (int offset = 8; offset + 60 <= buffer.Length;)
                {
                    int recordLength = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset, 4)));
                    if (recordLength < 60 || offset + recordLength > buffer.Length) return false;
                    ProcessRecord(buffer.AsSpan(offset, recordLength));
                    offset += recordLength;
                }
                current = next;
                BinaryPrimitives.WriteInt64LittleEndian(input.AsSpan(0, 8), current);
                Marshal.Copy(input, 0, inputPtr, input.Length);
                if (returned == 8) break;
            }
            return true;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or ArgumentException or OverflowException)
        {
            return false;
        }
        finally { Marshal.FreeHGlobal(inputPtr); Marshal.FreeHGlobal(outputPtr); }
    }

    private void ProcessRecord(ReadOnlySpan<byte> record)
    {
        ushort major = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(4, 2));
        if (major != 2) return;
        ulong fileId = BinaryPrimitives.ReadUInt64LittleEndian(record.Slice(8, 8));
        ulong parentId = BinaryPrimitives.ReadUInt64LittleEndian(record.Slice(16, 8));
        uint reason = BinaryPrimitives.ReadUInt32LittleEndian(record.Slice(40, 4));
        uint attrs = BinaryPrimitives.ReadUInt32LittleEndian(record.Slice(52, 4));
        ushort nameLength = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(56, 2));
        ushort nameOffset = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(58, 2));
        if (nameOffset + nameLength > record.Length || (nameLength & 1) != 0) return;
        string name = Encoding.Unicode.GetString(record.Slice(nameOffset, nameLength));
        bool isDirectory = (attrs & 0x10) != 0;
        bool oldName = (reason & 0x00001000) != 0;
        bool newName = (reason & 0x00002000) != 0;
        if (oldName)
        {
            string? oldPath = pathsById.TryGetValue(fileId, out List<string>? existing) ? existing.FirstOrDefault() : null;
            pendingRenames[fileId] = new PendingRename(parentId, name, oldPath, isDirectory);
            return;
        }
        if (newName)
        {
            pendingRenames.TryGetValue(fileId, out PendingRename pending);
            pendingRenames.Remove(fileId);
            string? oldPath = pending.OldPath ?? (pathsById.TryGetValue(fileId, out List<string>? existing) ? existing.FirstOrDefault() : null);
            string? parent = ParentPath(parentId);
            string? next = parent is null ? null : Path.Combine(parent, name);
            if (oldPath is not null && next is not null && PathIdentity.IsSameOrChild(root, next))
            {
                Emit(new FileSystemEvent(next, oldPath, FileSystemEventKind.Renamed, Reconcile: isDirectory));
                ReplacePath(fileId, oldPath, next);
            }
            return;
        }
        if ((reason & 0x00000200) != 0) // FILE_DELETE
        {
            if (pathsById.TryGetValue(fileId, out List<string>? paths))
                foreach (string path in paths.ToArray()) Emit(new FileSystemEvent(path, Kind: FileSystemEventKind.Deleted));
            pathsById.Remove(fileId);
            return;
        }
        if ((reason & 0x00000100) != 0 || // FILE_CREATE
            (reason & 0x00000001) != 0 || (reason & 0x00000002) != 0 || (reason & 0x00000004) != 0 ||
            (reason & 0x00000010) != 0 || (reason & 0x00000020) != 0)
        {
            if (pathsById.TryGetValue(fileId, out List<string>? paths))
                foreach (string path in paths.ToArray()) Emit(new FileSystemEvent(path, Kind: FileSystemEventKind.Changed));
            else if (ParentPath(parentId) is string parent && PathIdentity.IsSameOrChild(root, parent))
            {
                string path = Path.Combine(parent, name);
                Emit(new FileSystemEvent(path, Kind: FileSystemEventKind.Created, Reconcile: isDirectory));
                pathsById[fileId] = [path];
            }
        }
    }

    private string? ParentPath(ulong parentId) => pathsById.TryGetValue(parentId, out List<string>? paths) ? paths.FirstOrDefault() : null;
    private void Emit(FileSystemEvent change) { if (PathIdentity.IsSameOrChild(root, change.Path)) Changed?.Invoke(change); }
    private void ReplacePath(ulong id, string oldPath, string next)
    {
        if (!pathsById.TryGetValue(id, out List<string>? paths)) pathsById[id] = [next];
        else { for (int i = 0; i < paths.Count; i++) if (paths[i].Equals(oldPath, StringComparison.Ordinal)) paths[i] = next; }
    }

    private bool TryOpenVolume(out SafeFileHandle? handle)
    {
        handle = null;
        try
        {
            // FSCTL_QUERY_USN_JOURNAL requires a traversable volume handle even
            // when the caller only reads journal metadata.  Opening with desired
            // access 0 makes DeviceIoControl fail with ERROR_INVALID_FUNCTION on
            // a standard medium-integrity Windows process and silently forces the
            // expensive full reconcile fallback.
            const uint GenericExecute = 0x20000000;
            handle = CreateFileW("\\\\.\\" + volumeRoot.TrimEnd('\\'), GenericExecute,
                FileShareRead | FileShareWrite | FileShareDelete, IntPtr.Zero, 3, 0, IntPtr.Zero);
            return !handle.IsInvalid;
        }
        catch (DllNotFoundException) { return false; }
    }

    private static bool TryQueryJournal(SafeFileHandle handle, out JournalState state)
    {
        state = default;
        int size = Marshal.SizeOf<UsnJournalData>(); IntPtr output = Marshal.AllocHGlobal(size);
        try
        {
            if (!DeviceIoControl(handle, FsctlQueryUsnJournal, IntPtr.Zero, 0, output, size, out uint returned, IntPtr.Zero) || returned < size)
                return false;
            UsnJournalData data = Marshal.PtrToStructure<UsnJournalData>(output);
            state = new JournalState(data.UsnJournalId, data.FirstUsn, data.NextUsn);
            return state.NextUsn >= state.FirstUsn;
        }
        finally { Marshal.FreeHGlobal(output); }
    }

    private JournalCursor? ReadCursor()
    {
        try
        {
            if (!File.Exists(cursorPath)) return null;
            return JsonSerializer.Deserialize<JournalCursor>(File.ReadAllText(cursorPath));
        }
        catch (Exception ex) when (ex is IOException or JsonException) { return null; }
    }

    private void WriteCursor(JournalCursor cursor)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(cursorPath)!);
        string temp = cursorPath + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(cursor), new UTF8Encoding(false));
        File.Move(temp, cursorPath, true);
    }

    private readonly record struct JournalState(ulong JournalId, long FirstUsn, long NextUsn);
    private readonly record struct JournalCursor(ulong JournalId, long NextUsn);
    private readonly record struct PendingRename(ulong ParentId, string OldName, string? OldPath, bool IsDirectory);

    private const uint FileShareRead = 1, FileShareWrite = 2, FileShareDelete = 4;
    [StructLayout(LayoutKind.Sequential, Pack = 1)] private struct UsnJournalData
    {
        public ulong UsnJournalId; public long FirstUsn; public long NextUsn; public long LowestValidUsn;
        public long MaxUsn; public ulong MaximumSize; public ulong AllocationDelta; public ushort MinSupportedMajorVersion; public ushort MaxSupportedMajorVersion;
    }
    [StructLayout(LayoutKind.Sequential, Pack = 1)] private struct ReadUsnJournalData
    {
        public long StartUsn; public uint ReasonMask; public uint ReturnOnlyOnClose; public ulong Timeout;
        public ulong BytesToWaitFor; public ulong UsnJournalId; public ushort MinMajorVersion; public ushort MaxMajorVersion;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DeviceIoControl(SafeFileHandle device, uint code, IntPtr input, int inputSize, IntPtr output, int outputSize, out uint returned, IntPtr overlapped);
}

internal static class VolumeChangeFeedFactory
{
    public static IVolumeChangeFeed Create(string root, string store)
    {
        try
        {
            string? driveRoot = Path.GetPathRoot(Path.GetFullPath(root));
            if (OperatingSystem.IsWindows() && driveRoot is not null)
            {
                var drive = new DriveInfo(driveRoot);
                if (drive.IsReady && drive.DriveFormat.Equals("NTFS", StringComparison.OrdinalIgnoreCase))
                {
                    string cursor = Path.Combine(Path.GetFullPath(store) + ".data", "usn.cursor");
                    var usn = new UsnVolumeChangeFeed(root, cursor);
                    if (usn.Probe()) return usn;
                    usn.Dispose();
                }
            }
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or PlatformNotSupportedException) { }
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

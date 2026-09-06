using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
using System.Security.Cryptography;
using System.Text;

namespace PersonalRag.FilenameSearch;

internal static class PathIdentity
{
    public static string FullExact(string path) => Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));

    public static bool IsSameOrChild(string root, string path)
    {
        root = Path.GetFullPath(root);
        path = Path.GetFullPath(path);
        if (path.Equals(root, StringComparison.OrdinalIgnoreCase)) return true;
        string prefix = Path.EndsInDirectorySeparator(root) ? root : root + Path.DirectorySeparatorChar;
        return path.StartsWith(prefix, StringComparison.OrdinalIgnoreCase);
    }
}

internal static class VolumeIdentity
{
    public static string GetVolumeId(string path)
    {
        string root = Path.GetPathRoot(Path.GetFullPath(path)) ?? throw new ArgumentException("Path has no root", nameof(path));
        if (OperatingSystem.IsWindows())
        {
            var buffer = new StringBuilder(128);
            if (GetVolumeNameForVolumeMountPointW(root, buffer, buffer.Capacity))
                return buffer.ToString().TrimEnd('\\').ToUpperInvariant();
        }
        return root.ToUpperInvariant();
    }

    public static FileKey GetFileKey(string path, bool isDirectory) =>
        GetFileKey(path, isDirectory, GetVolumeId(path));

    public static FileKey GetFileKey(string path, bool isDirectory, string volumeId)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(volumeId);
        if (OperatingSystem.IsWindows() && TryGetNativeId(path, isDirectory, out ulong nativeId))
            return new FileKey(volumeId, nativeId, true);

        byte[] bytes = SHA256.HashData(Encoding.UTF8.GetBytes(Path.GetFullPath(path)));
        ulong fallback = BitConverter.ToUInt64(bytes, 0);
        return new FileKey(volumeId, fallback, false);
    }

    private static bool TryGetNativeId(string path, bool isDirectory, out ulong id)
    {
        id = 0;
        uint flags = isDirectory ? FILE_FLAG_BACKUP_SEMANTICS : 0u;
        using SafeFileHandle handle = CreateFileW(
            path,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            flags,
            IntPtr.Zero);
        if (handle.IsInvalid) return false;
        if (!GetFileInformationByHandle(handle, out ByHandleFileInformation info)) return false;
        id = ((ulong)info.FileIndexHigh << 32) | info.FileIndexLow;
        return id != 0;
    }

    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFileW(
        string lpFileName,
        uint dwDesiredAccess,
        uint dwShareMode,
        IntPtr lpSecurityAttributes,
        uint dwCreationDisposition,
        uint dwFlagsAndAttributes,
        IntPtr hTemplateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetFileInformationByHandle(
        SafeFileHandle hFile,
        out ByHandleFileInformation lpFileInformation);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetVolumeNameForVolumeMountPointW(
        string lpszVolumeMountPoint,
        StringBuilder lpszVolumeName,
        int cchBufferLength);

    [StructLayout(LayoutKind.Sequential)]
    private struct ByHandleFileInformation
    {
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }
}

internal sealed record VolumeDescriptor(string Root, string VolumeId, string DriveFormat);

internal static class LocalVolumeDiscovery
{
    public static IReadOnlyList<VolumeDescriptor> FixedReadyVolumes()
    {
        var result = new List<VolumeDescriptor>();
        foreach (DriveInfo drive in DriveInfo.GetDrives())
        {
            try
            {
                if (!drive.IsReady || drive.DriveType != DriveType.Fixed) continue;
                // Cloud-backed virtual drives can report DriveType.Fixed even though they
                // are not local filesystem volumes (for example, Google Drive). They must
                // not be federated by the rootless GUI or counted as a second fixed local
                // volume. Physical fixed disks keep their normal volume labels.
                string label = drive.VolumeLabel;
                if (label.Contains("google drive", StringComparison.OrdinalIgnoreCase) ||
                    label.Contains("onedrive", StringComparison.OrdinalIgnoreCase) ||
                    label.Contains("dropbox", StringComparison.OrdinalIgnoreCase)) continue;
                string root = Path.GetFullPath(drive.RootDirectory.FullName);
                result.Add(new VolumeDescriptor(root, VolumeIdentity.GetVolumeId(root), drive.DriveFormat));
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }
        return result.OrderBy(v => v.Root, StringComparer.OrdinalIgnoreCase).ToArray();
    }
}

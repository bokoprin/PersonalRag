using System.IO.Compression;
using System.Security.Cryptography;
using System.Text;

namespace Astra.Core;

public static class IndexStore
{
    private static readonly byte[] Magic = "ASTRA002"u8.ToArray();
    public static IndexWriteLease AcquireWriter(string directory) => new(directory);
    public static void Save(string directory, IndexSnapshot snapshot, IndexWriteLease? lease = null)
    {
        directory = Path.GetFullPath(directory);
        if (directory.Equals(snapshot.Root, StringComparison.OrdinalIgnoreCase) ||
            directory.StartsWith(Path.TrimEndingDirectorySeparator(snapshot.Root) + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase))
            throw new ArgumentException("Index directory must be outside the source root");
        Directory.CreateDirectory(directory);
        using var localLease = lease is null ? AcquireWriter(directory) : null;
        (lease ?? localLease!).Validate(directory);
        string temp = Path.Combine(directory, "snapshot-" + Guid.NewGuid().ToString("N") + ".tmp");
        try
        {
            using (var stream = new FileStream(temp, FileMode.CreateNew, FileAccess.ReadWrite, FileShare.None))
            {
                stream.Write(Magic); stream.Write(new byte[32]);
                using (var compressed = new BrotliStream(stream, CompressionLevel.Fastest, true))
                using (var writer = new BinaryWriter(compressed, Encoding.UTF8, true))
                {
                    writer.Write(snapshot.Root); writer.Write(snapshot.CreatedUtc.Ticks); writer.Write(snapshot.Files.Length);
                    foreach (var f in snapshot.Files)
                    {
                        writer.Write(Path.GetRelativePath(snapshot.Root, f.Path)); writer.Write(f.Size); writer.Write(f.ModifiedUtcTicks);
                        writer.Write(f.Unsearchable ?? ""); writer.Write(f.Signature.Length); writer.Write(f.Signature);
                    }
                }
                stream.Position = 40;
                var hash = SHA256.HashData(stream);
                stream.Position = 8; stream.Write(hash); stream.Flush(true);
            }
            File.Move(temp, Path.Combine(directory, "snapshot.astra"), true);
            // A sole writer can safely remove only abandoned files belonging to this store format.
            foreach (string abandoned in Directory.EnumerateFiles(directory, "snapshot-*.tmp"))
            {
                string name = Path.GetFileNameWithoutExtension(abandoned);
                if (name.Length == 41 && Guid.TryParseExact(name[9..], "N", out _)) File.Delete(abandoned);
            }
        }
        finally { if (File.Exists(temp)) File.Delete(temp); }
    }
    public static IndexSnapshot Load(string directory)
    {
        using var stream = new FileStream(Path.Combine(directory, "snapshot.astra"), FileMode.Open, FileAccess.Read, FileShare.Read | FileShare.Delete);
        var header = new byte[40]; stream.ReadExactly(header);
        if (!header.AsSpan(0, 8).SequenceEqual(Magic) || !CryptographicOperations.FixedTimeEquals(header.AsSpan(8), SHA256.HashData(stream)))
            throw new InvalidDataException("Index checksum or version mismatch");
        stream.Position = 40;
        using var compressed = new BrotliStream(stream, CompressionMode.Decompress);
        using var reader = new BinaryReader(compressed, Encoding.UTF8);
        string root = reader.ReadString(); var created = new DateTime(reader.ReadInt64(), DateTimeKind.Utc);
        int count = reader.ReadInt32();
        if (count < 0 || count > 10_000_000) throw new InvalidDataException("Invalid file count");
        var entries = new FileEntry[count];
        for (int i = 0; i < count; i++)
        {
            string relative = reader.ReadString();
            string path = Path.GetFullPath(Path.Combine(root, relative));
            if (!path.StartsWith(Path.TrimEndingDirectorySeparator(root) + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase))
                throw new InvalidDataException("Index path escapes root");
            long size = reader.ReadInt64(), ticks = reader.ReadInt64(); string error = reader.ReadString();
            int length = reader.ReadInt32();
            if (length < 0 || length > 65536 || (length > 0 && (length & (length - 1)) != 0)) throw new InvalidDataException("Invalid signature length");
            var bits = reader.ReadBytes(length);
            if (bits.Length != length) throw new EndOfStreamException();
            entries[i] = new FileEntry(path, size, ticks, error.Length == 0 ? null : error, bits);
        }
        if (reader.BaseStream.ReadByte() != -1) throw new InvalidDataException("Unexpected trailing payload");
        return new IndexSnapshot(root, entries, created);
    }
    public static long PersistentBytes(string directory) => Directory.EnumerateFiles(directory, "*", SearchOption.AllDirectories)
        .Sum(path => new FileInfo(path).Length);
}

public sealed class IndexWriteLease : IDisposable
{
    private readonly string directory;
    private FileStream? handle;
    internal IndexWriteLease(string directory)
    {
        this.directory = Path.GetFullPath(directory); Directory.CreateDirectory(this.directory);
        handle = new FileStream(Path.Combine(this.directory, "writer.lock"), FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
    }
    internal void Validate(string value)
    {
        ObjectDisposedException.ThrowIf(handle is null, this);
        if (!directory.Equals(Path.GetFullPath(value), StringComparison.OrdinalIgnoreCase)) throw new ArgumentException("Writer lease belongs to another store");
    }
    public void Dispose() { handle?.Dispose(); handle = null; }
}

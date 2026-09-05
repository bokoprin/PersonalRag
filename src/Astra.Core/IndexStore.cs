using System.IO.Compression;
using System.Security.Cryptography;
using System.Text;

namespace Astra.Core;

public static class IndexStore
{
    private static readonly byte[] Magic = "ASTRA003"u8.ToArray();
    private const int FilesPerBlock = 256;
    private const int MaximumBlockBytes = 128 * 1024 * 1024;
    private record Block(long Offset, int CompressedLength, int RawLength, byte[] Hash);
    public static IndexWriteLease AcquireWriter(string directory) => new(directory);

    public static void Save(string directory, IndexSnapshot snapshot, IndexWriteLease? lease = null, Action? beforeCommit = null)
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
                var blocks = Enumerable.Range(0, (snapshot.Files.Length + FilesPerBlock - 1) / FilesPerBlock)
                    .Select(_ => new Block(0, 0, 0, new byte[32])).ToArray();
                byte[] header = Header(snapshot.Root, snapshot.CreatedUtc, snapshot.Files.Length, blocks);
                stream.Write(Magic);
                using var fileWriter = new BinaryWriter(stream, Encoding.UTF8, true);
                fileWriter.Write(header.Length); fileWriter.Write(new byte[32]); fileWriter.Write(header);
                object append = new();
                Parallel.For(0, blocks.Length, new ParallelOptions { MaxDegreeOfParallelism = Math.Min(8, Environment.ProcessorCount) }, index =>
                {
                    using var raw = new MemoryStream();
                    using (var writer = new BinaryWriter(raw, Encoding.UTF8, true))
                    {
                        int end = Math.Min(snapshot.Files.Length, (index + 1) * FilesPerBlock);
                        for (int i = index * FilesPerBlock; i < end; i++)
                        {
                            var f = snapshot.Files[i];
                            writer.Write(Path.GetRelativePath(snapshot.Root, f.Path)); writer.Write(f.Size); writer.Write(f.ModifiedUtcTicks);
                            writer.Write(f.Unsearchable ?? ""); writer.Write(f.Signature.Length); writer.Write(f.Signature.Span);
                        }
                    }
                    if (raw.Length > MaximumBlockBytes) throw new InvalidDataException("Index block exceeds format limit");
                    using var compressed = new MemoryStream();
                    using (var compressor = new BrotliStream(compressed, CompressionLevel.Fastest, true))
                        compressor.Write(raw.GetBuffer().AsSpan(0, (int)raw.Length));
                    var data = compressed.GetBuffer().AsSpan(0, (int)compressed.Length);
                    byte[] hash = SHA256.HashData(data);
                    lock (append)
                    {
                        blocks[index] = new Block(stream.Position, data.Length, (int)raw.Length, hash);
                        stream.Write(data);
                    }
                });
                beforeCommit?.Invoke();
                header = Header(snapshot.Root, snapshot.CreatedUtc, snapshot.Files.Length, blocks);
                stream.Position = 12; stream.Write(SHA256.HashData(header)); stream.Write(header); stream.Flush(true);
            }
            File.Move(temp, Path.Combine(directory, "snapshot.astra"), true);
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
        using var stream = new FileStream(Path.Combine(directory, "snapshot.astra"), FileMode.Open, FileAccess.Read,
            FileShare.Read | FileShare.Delete, 4096, FileOptions.RandomAccess);
        using var fileReader = new BinaryReader(stream, Encoding.UTF8, true);
        if (!fileReader.ReadBytes(8).AsSpan().SequenceEqual(Magic)) throw new InvalidDataException("Index version mismatch");
        int headerLength = fileReader.ReadInt32();
        if (headerLength < 0 || headerLength > 4 * 1024 * 1024) throw new InvalidDataException("Invalid header length");
        byte[] hash = fileReader.ReadBytes(32), header = fileReader.ReadBytes(headerLength);
        if (header.Length != headerLength || hash.Length != 32 || !CryptographicOperations.FixedTimeEquals(hash, SHA256.HashData(header)))
            throw new InvalidDataException("Index header checksum mismatch");
        using var headerStream = new MemoryStream(header, false);
        using var reader = new BinaryReader(headerStream, Encoding.UTF8);
        string root = reader.ReadString(); var created = new DateTime(reader.ReadInt64(), DateTimeKind.Utc);
        int count = reader.ReadInt32(), blockCount = reader.ReadInt32();
        if (count < 0 || count > 10_000_000 || blockCount != (count + FilesPerBlock - 1) / FilesPerBlock)
            throw new InvalidDataException("Invalid file/block count");
        var blocks = new Block[blockCount];
        for (int i = 0; i < blocks.Length; i++)
        {
            long offset = reader.ReadInt64(); int compressed = reader.ReadInt32(), raw = reader.ReadInt32(); byte[] digest = reader.ReadBytes(32);
            if (offset < 44L + headerLength || compressed <= 0 || compressed > MaximumBlockBytes || raw <= 0 || raw > MaximumBlockBytes ||
                offset > stream.Length - compressed || digest.Length != 32) throw new InvalidDataException("Invalid block extent");
            blocks[i] = new Block(offset, compressed, raw, digest);
        }
        if (headerStream.Position != headerStream.Length) throw new InvalidDataException("Trailing header data");
        long extent = 44L + headerLength;
        foreach (var block in blocks.OrderBy(b => b.Offset))
        { if (block.Offset != extent) throw new InvalidDataException("Overlapping or missing block"); extent += block.CompressedLength; }
        if (extent != stream.Length) throw new InvalidDataException("Trailing index data");
        var entries = new FileEntry[count];
        // BinaryReader may leave a read-ahead buffer on the stream. Capture the
        // handle once, before entering parallel reads; SafeFileHandle access
        // itself can seek to flush that buffer and is not safe to race.
        var fileHandle = stream.SafeFileHandle;
        try
        {
            Parallel.For(0, blocks.Length, new ParallelOptions { MaxDegreeOfParallelism = Math.Min(8, Environment.ProcessorCount) }, index =>
            {
                var block = blocks[index];
                byte[] compressed = GC.AllocateUninitializedArray<byte>(block.CompressedLength);
                int read = 0;
                while (read < compressed.Length)
                {
                    int n = RandomAccess.Read(fileHandle, compressed.AsSpan(read), block.Offset + read);
                    if (n == 0) throw new EndOfStreamException(); read += n;
                }
                if (!CryptographicOperations.FixedTimeEquals(block.Hash, SHA256.HashData(compressed))) throw new InvalidDataException("Index block checksum mismatch");
                byte[] raw = GC.AllocateUninitializedArray<byte>(block.RawLength);
                if (!BrotliDecoder.TryDecompress(compressed, raw, out int written) || written != raw.Length) throw new InvalidDataException("Invalid compressed block");
                using var data = new MemoryStream(raw, false);
                using var records = new BinaryReader(data, Encoding.UTF8);
                int end = Math.Min(count, (index + 1) * FilesPerBlock);
                for (int i = index * FilesPerBlock; i < end; i++)
                {
                    string relative = records.ReadString();
                    string path = Path.GetFullPath(Path.Combine(root, relative));
                    if (!path.StartsWith(Path.TrimEndingDirectorySeparator(root) + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase))
                        throw new InvalidDataException("Index path escapes root");
                    long size = records.ReadInt64(), ticks = records.ReadInt64(); string error = records.ReadString();
                    int length = records.ReadInt32();
                    if (length < 0 || length > 65536 || (length > 0 && (length & (length - 1)) != 0) || data.Position + length > data.Length)
                        throw new InvalidDataException("Invalid signature length");
                    var signature = raw.AsMemory((int)data.Position, length); data.Position += length;
                    entries[i] = new FileEntry(path, size, ticks, error.Length == 0 ? null : error, signature);
                }
                if (data.Position != data.Length) throw new InvalidDataException("Trailing block data");
            });
        }
        catch (AggregateException ex) when (ex.Flatten().InnerExceptions.All(e => e is InvalidDataException or EndOfStreamException or DecoderFallbackException))
        { throw new InvalidDataException("Index block validation failed", ex); }
        for (int i = 1; i < entries.Length; i++)
            if (StringComparer.OrdinalIgnoreCase.Compare(entries[i - 1].Path, entries[i].Path) >= 0) throw new InvalidDataException("Duplicate or unsorted index paths");
        return new IndexSnapshot(root, entries, created);
    }

    private static byte[] Header(string root, DateTime created, int count, Block[] blocks)
    {
        using var stream = new MemoryStream(); using var writer = new BinaryWriter(stream, Encoding.UTF8, true);
        writer.Write(root); writer.Write(created.Ticks); writer.Write(count); writer.Write(blocks.Length);
        foreach (var block in blocks) { writer.Write(block.Offset); writer.Write(block.CompressedLength); writer.Write(block.RawLength); writer.Write(block.Hash); }
        return stream.ToArray();
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

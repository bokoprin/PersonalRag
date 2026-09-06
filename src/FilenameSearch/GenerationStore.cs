using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace PersonalRag.FilenameSearch;

/// <summary>Manifest + immutable base + checksummed append-only delta persistence.</summary>
internal sealed class GenerationStore : IDisposable
{
    private const int FormatVersion = 3;
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private readonly string manifestPath;
    private readonly string dataDir;
    private readonly string rootIdentity;
    private readonly string lockPath;
    private readonly string dirtyPath;
    private readonly string manifestShaPath;
    private readonly FileStream lease;
    private Manifest? manifest;
    private long journalEntries;
    private long persistenceWriteBytes;
    private long fullBaseRewriteCount;
    private bool clean;

    public GenerationStore(string rootIdentity, string storePath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(rootIdentity);
        ArgumentException.ThrowIfNullOrWhiteSpace(storePath);
        this.rootIdentity = rootIdentity;
        manifestPath = Path.GetFullPath(storePath);
        Directory.CreateDirectory(Path.GetDirectoryName(manifestPath)!);
        dataDir = manifestPath + ".data";
        Directory.CreateDirectory(dataDir);
        lockPath = Path.Combine(dataDir, "writer.lock");
        dirtyPath = Path.Combine(dataDir, "dirty.marker");
        manifestShaPath = Path.Combine(dataDir, "manifest.sha256");
        WasDirtyShutdown = File.Exists(dirtyPath);
        lease = new FileStream(lockPath, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
        CleanupTemps();
        File.WriteAllText(dirtyPath, $"{Environment.ProcessId}|{DateTime.UtcNow:O}", Encoding.UTF8);
        Flush(dirtyPath);
    }

    public bool WasDirtyShutdown { get; }
    public long PersistedGeneration => manifest?.LastGeneration ?? 0;
    internal long PersistenceWriteBytes => Interlocked.Read(ref persistenceWriteBytes);
    internal long FullBaseRewriteCount => Interlocked.Read(ref fullBaseRewriteCount);
    internal long DeltaChangeCount => Interlocked.Read(ref journalEntries);
    internal long BaseGeneration => manifest?.BaseGeneration ?? 0;
    internal long DeltaBytes
    {
        get
        {
            Manifest? current = manifest;
            return current is null ? 0 : TryLength(Data(current.DeltaFile));
        }
    }
    public bool ShouldCompact
    {
        get
        {
            if (manifest is null) return false;
            long bytes = TryLength(Data(manifest.DeltaFile));
            return Interlocked.Read(ref journalEntries) >= 4096 || bytes >= 16L * 1024 * 1024;
        }
    }

    public bool TryLoad(FilenameSearchEngine engine, out long generation)
    {
        ArgumentNullException.ThrowIfNull(engine);
        generation = 0;
        if (!File.Exists(manifestPath)) return false;
        Manifest current = ReadManifest();
        string index = Data(current.BaseIndexFile);
        string meta = Data(current.BaseMetadataFile);
        string delta = Data(current.DeltaFile);
        VerifyFile(index, current.BaseIndexSha256);
        VerifyFile(meta, current.BaseMetadataSha256);
        if (!File.Exists(delta)) throw new InvalidDataException("Filename delta journal is missing");
        engine.LoadBase(index, ReadMetadata(meta));
        generation = current.BaseGeneration;
        long replayed = 0;
        foreach (CatalogChangeBatch batch in ReadDelta(delta))
        {
            Apply(engine, batch);
            generation = Math.Max(generation, batch.Generation);
            replayed += Math.Max(1, batch.Changes.Count);
        }
        manifest = current with { LastGeneration = generation };
        Interlocked.Exchange(ref journalEntries, replayed);
        return true;
    }

    public void InitializeBase(FilenameSearchEngine compacted, IReadOnlyList<FilenameRecord> records, long generation) =>
        PublishBase(compacted, records, generation);

    public void CommitCompaction(FilenameSearchEngine compacted, IReadOnlyList<FilenameRecord> records, long generation) =>
        PublishBase(compacted, records, generation);

    public void Append(CatalogChangeBatch batch)
    {
        if (batch.Changes.Count == 0) return;
        Manifest current = manifest ?? throw new InvalidOperationException("Store base is not initialized");
        string payload = JsonSerializer.Serialize(batch, Json);
        string line = Sha(Encoding.UTF8.GetBytes(payload)) + "\t" + payload + Environment.NewLine;
        string delta = Data(current.DeltaFile);
        using var stream = new FileStream(delta, FileMode.Append, FileAccess.Write, FileShare.Read);
        byte[] bytes = Encoding.UTF8.GetBytes(line);
        stream.Write(bytes);
        stream.Flush(true);
        Interlocked.Add(ref persistenceWriteBytes, bytes.Length);
        Interlocked.Add(ref journalEntries, Math.Max(1, batch.Changes.Count));
        manifest = current with { LastGeneration = Math.Max(current.LastGeneration, batch.Generation) };
    }

    public void ResetCorruptStore()
    {
        manifest = null;
        Interlocked.Exchange(ref journalEntries, 0);
        TryDelete(manifestPath);
        TryDelete(manifestShaPath);
        foreach (string file in Directory.EnumerateFiles(dataDir))
        {
            string name = Path.GetFileName(file);
            if (name.Equals("writer.lock", StringComparison.OrdinalIgnoreCase) ||
                name.Equals("dirty.marker", StringComparison.OrdinalIgnoreCase)) continue;
            TryDelete(file);
        }
    }

    public void MarkClean()
    {
        if (clean) return;
        TryDelete(dirtyPath);
        clean = true;
    }

    public static long GetPersistentBytes(string storePath)
    {
        string full = Path.GetFullPath(storePath);
        long total = File.Exists(full) ? TryLength(full) : 0;
        string data = full + ".data";
        if (Directory.Exists(data))
            foreach (string file in Directory.EnumerateFiles(data, "*", SearchOption.AllDirectories))
                total = checked(total + TryLength(file));
        return total;
    }

    public void Dispose() => lease.Dispose();

    private void PublishBase(FilenameSearchEngine compacted, IReadOnlyList<FilenameRecord> records, long generation)
    {
        if (compacted.OverlayCount != 0) throw new InvalidOperationException("Base publish requires a compacted engine");
        string token = $"{generation:D20}-{Guid.NewGuid():N}";
        string indexName = $"base-{token}.routec";
        string metaName = $"base-{token}.meta";
        string deltaName = $"delta-{token}.log";
        string index = Data(indexName), meta = Data(metaName), delta = Data(deltaName);
        string indexTmp = index + ".tmp", metaTmp = meta + ".tmp", deltaTmp = delta + ".tmp";
        string manifestTmp = manifestPath + ".tmp";
        try
        {
            compacted.SaveBase(indexTmp); Flush(indexTmp);
            WriteMetadata(metaTmp, records); Flush(metaTmp);
            using (var empty = new FileStream(deltaTmp, FileMode.Create, FileAccess.Write, FileShare.Read)) empty.Flush(true);
            File.Move(indexTmp, index); File.Move(metaTmp, meta); File.Move(deltaTmp, delta);
            var next = new Manifest(
                FormatVersion, rootIdentity, global::FilenameSearch.Core.FilenameSemantics.NormalizerVersion,
                generation, generation, indexName, ShaFile(index), metaName, ShaFile(meta), deltaName);
            File.WriteAllText(manifestTmp, JsonSerializer.Serialize(next, Json), new UTF8Encoding(false));
            Flush(manifestTmp);
            File.Move(manifestTmp, manifestPath, overwrite: true);
            File.WriteAllText(manifestShaPath, ShaFile(manifestPath), new UTF8Encoding(false));
            Flush(manifestShaPath);
            Interlocked.Add(ref persistenceWriteBytes,
                TryLength(index) + TryLength(meta) + TryLength(delta) +
                TryLength(manifestPath) + TryLength(manifestShaPath));
            Interlocked.Increment(ref fullBaseRewriteCount);
            manifest = next;
            Interlocked.Exchange(ref journalEntries, 0);
            CleanupOld(next);
        }
        finally
        {
            TryDelete(indexTmp); TryDelete(metaTmp); TryDelete(deltaTmp); TryDelete(manifestTmp);
        }
    }

    private Manifest ReadManifest()
    {
        VerifyManifestChecksum();
        Manifest value = JsonSerializer.Deserialize<Manifest>(File.ReadAllText(manifestPath, Encoding.UTF8), Json)
            ?? throw new InvalidDataException("Filename manifest is empty");
        if (value.Version != FormatVersion) throw new InvalidDataException("Filename manifest version mismatch");
        if (!value.RootIdentity.Equals(rootIdentity, StringComparison.Ordinal))
            throw new InvalidDataException("Filename store root identity mismatch");
        if (!value.NormalizerVersion.Equals(global::FilenameSearch.Core.FilenameSemantics.NormalizerVersion, StringComparison.Ordinal))
            throw new InvalidDataException("Filename normalizer version mismatch");
        if (value.BaseGeneration < 0 || value.LastGeneration < value.BaseGeneration)
            throw new InvalidDataException("Filename manifest generation is invalid");
        ValidateLeaf(value.BaseIndexFile); ValidateLeaf(value.BaseMetadataFile); ValidateLeaf(value.DeltaFile);
        return value;
    }

    private IEnumerable<CatalogChangeBatch> ReadDelta(string path)
    {
        string[] lines = File.ReadAllLines(path, Encoding.UTF8);
        for (int i = 0; i < lines.Length; i++)
        {
            string line = lines[i];
            if (line.Length == 0) continue;
            int tab = line.IndexOf('\t');
            bool tornTail = WasDirtyShutdown && i == lines.Length - 1;
            if (tab <= 0 || tab == line.Length - 1)
            {
                if (tornTail) yield break;
                throw new InvalidDataException($"Malformed delta record {i + 1}");
            }
            string payload = line[(tab + 1)..];
            if (!line[..tab].Equals(Sha(Encoding.UTF8.GetBytes(payload)), StringComparison.OrdinalIgnoreCase))
            {
                if (tornTail) yield break;
                throw new InvalidDataException($"Delta checksum mismatch at record {i + 1}");
            }
            yield return JsonSerializer.Deserialize<CatalogChangeBatch>(payload, Json)
                ?? throw new InvalidDataException($"Empty delta record {i + 1}");
        }
    }

    private static void Apply(FilenameSearchEngine engine, CatalogChangeBatch batch)
    {
        foreach (CatalogChange change in batch.Changes)
            if (change.After is not null) engine.Upsert(change.After);
            else if (change.Before is not null) engine.Remove(change.Before.FileId);
    }

    private static void WriteMetadata(string path, IReadOnlyList<FilenameRecord> records)
    {
        using var stream = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.Read);
        using var writer = new BinaryWriter(stream, new UTF8Encoding(false), leaveOpen: true);
        writer.Write("PRFMETA3"); writer.Write(records.Count);
        foreach (FilenameRecord r in records.OrderBy(r => r.FileId))
        {
            writer.Write(r.FileId); writer.Write(r.ParentId ?? 0); writer.Write(r.ParentId.HasValue);
            WriteKey(writer, r.Key); writer.Write(r.ParentKey.HasValue); if (r.ParentKey is FileKey pk) WriteKey(writer, pk);
            writer.Write(r.SizeBytes); writer.Write(r.ModifiedUtc.Ticks); writer.Write(r.Flags); writer.Write(r.Name); writer.Write(r.FullPath);
        }
        writer.Flush(); stream.Flush(true);
    }

    private static FilenameRecord[] ReadMetadata(string path)
    {
        using var stream = File.OpenRead(path);
        using var reader = new BinaryReader(stream, Encoding.UTF8, leaveOpen: true);
        if (reader.ReadString() != "PRFMETA3") throw new InvalidDataException("Filename metadata header mismatch");
        int count = reader.ReadInt32();
        if (count < 0 || count > 20_000_000) throw new InvalidDataException("Filename metadata count invalid");
        var result = new FilenameRecord[count];
        for (int i = 0; i < count; i++)
        {
            int id = reader.ReadInt32(), parent = reader.ReadInt32(); bool hasParent = reader.ReadBoolean();
            FileKey key = ReadKey(reader); FileKey? parentKey = reader.ReadBoolean() ? ReadKey(reader) : null;
            ulong size = reader.ReadUInt64(); long ticks = reader.ReadInt64(); byte flags = reader.ReadByte();
            string name = reader.ReadString(), fullPath = reader.ReadString();
            result[i] = new FilenameRecord(id, hasParent ? parent : null, name, fullPath, size,
                new DateTime(ticks, DateTimeKind.Utc), flags) { Key = key, ParentKey = parentKey };
        }
        if (stream.Position != stream.Length) throw new InvalidDataException("Filename metadata trailing bytes");
        return result;
    }

    private static void WriteKey(BinaryWriter w, FileKey k) { w.Write(k.VolumeId); w.Write(k.NativeId); w.Write(k.IsNative); }
    private static FileKey ReadKey(BinaryReader r) => new(r.ReadString(), r.ReadUInt64(), r.ReadBoolean());
    private string Data(string leaf) => Path.Combine(dataDir, leaf);
    private static void ValidateLeaf(string leaf)
    {
        if (string.IsNullOrWhiteSpace(leaf) || Path.GetFileName(leaf) != leaf) throw new InvalidDataException("Unsafe manifest filename");
    }

    private void VerifyManifestChecksum()
    {
        if (!File.Exists(manifestShaPath)) throw new InvalidDataException("Filename manifest checksum is missing");
        string expected = File.ReadAllText(manifestShaPath, Encoding.UTF8).Trim();
        if (expected.Length != 64 || !ShaFile(manifestPath).Equals(expected, StringComparison.OrdinalIgnoreCase))
            throw new InvalidDataException("Filename manifest checksum mismatch");
    }

    private static void VerifyFile(string path, string expected)
    {
        if (!File.Exists(path) || !ShaFile(path).Equals(expected, StringComparison.OrdinalIgnoreCase))
            throw new InvalidDataException("Filename store checksum mismatch: " + Path.GetFileName(path));
    }
    private void CleanupTemps()
    {
        TryDelete(manifestPath + ".tmp");
        foreach (string file in Directory.EnumerateFiles(dataDir, "*.tmp")) TryDelete(file);
    }
    private void CleanupOld(Manifest keep)
    {
        var names = new HashSet<string>(StringComparer.OrdinalIgnoreCase)
        { "writer.lock", "dirty.marker", "manifest.sha256", keep.BaseIndexFile, keep.BaseMetadataFile, keep.DeltaFile };
        foreach (string file in Directory.EnumerateFiles(dataDir)) if (!names.Contains(Path.GetFileName(file))) TryDelete(file);
    }
    private static long TryLength(string path) { try { return new FileInfo(path).Length; } catch { return 0; } }
    private static string ShaFile(string path) { using var s = File.OpenRead(path); return Convert.ToHexString(SHA256.HashData(s)).ToLowerInvariant(); }
    private static string Sha(ReadOnlySpan<byte> bytes) => Convert.ToHexString(SHA256.HashData(bytes)).ToLowerInvariant();
    private static void Flush(string path) { using var s = new FileStream(path, FileMode.Open, FileAccess.ReadWrite, FileShare.Read); s.Flush(true); }
    private static void TryDelete(string path) { try { if (File.Exists(path)) File.Delete(path); } catch (IOException) { } catch (UnauthorizedAccessException) { } }

    private sealed record Manifest(
        int Version, string RootIdentity, string NormalizerVersion, long BaseGeneration, long LastGeneration,
        string BaseIndexFile, string BaseIndexSha256, string BaseMetadataFile, string BaseMetadataSha256, string DeltaFile);
}

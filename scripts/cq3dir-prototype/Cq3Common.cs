using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;

public struct ProtoMeta {
    public bool Found;
    public int Encoding;
    public uint Count;
    public uint Offset;
    public uint Bytes;

    public ulong Digest() {
        unchecked {
            ulong value = Found ? 0x9e3779b97f4a7c15UL : 0xd1b54a32d192ed03UL;
            value ^= (ulong)(uint)Encoding * 0x94d049bb133111ebUL;
            value ^= (ulong)Count * 0xbf58476d1ce4e5b9UL;
            value ^= ((ulong)Offset << 17) | ((ulong)Offset >> 47);
            value ^= ((ulong)Bytes << 31) | ((ulong)Bytes >> 33);
            return value;
        }
    }

    public bool SameAs(ProtoMeta other) {
        return Found == other.Found
            && Encoding == other.Encoding
            && Count == other.Count
            && Offset == other.Offset
            && Bytes == other.Bytes;
    }
}

public interface ICq3PrototypeLookup {
    string Name { get; }
    long StorageBytes { get; }
    bool TryLookup(uint key, out ProtoMeta meta);
}

public sealed class RepresentationSummary {
    public string Name { get; set; }
    public long DirectoryBytes { get; set; }
    public double DirectoryMiB { get; set; }
    public double DirectoryGiB { get; set; }
    public double DirectoryReductionPercent { get; set; }
    public long EstimatedWholeIndexBytes { get; set; }
    public double EstimatedWholeIndexGiB { get; set; }
    public double EstimatedWholeIndexReductionPercent { get; set; }
    public double PrototypeEncodeMs { get; set; }
    public string LookupModel { get; set; }
}

public sealed class AggregateTiming {
    public string Representation { get; set; }
    public string Workload { get; set; }
    public long TotalMeasuredOperations { get; set; }
    public int SegmentRuns { get; set; }
    public int RepeatsPerSegment { get; set; }
    public double MedianRunNsPerOp { get; set; }
    public double MinRunNsPerOp { get; set; }
    public double MaxRunNsPerOp { get; set; }
    public double BatchP50NsPerOp { get; set; }
    public double BatchP95NsPerOp { get; set; }
    public double BatchP99NsPerOp { get; set; }
    public double MedianMillionOpsPerSecond { get; set; }
    public double RatioVsCurrentMedian { get; set; }
    public ulong ChecksumXor { get; set; }
}

public sealed class SegmentSummary {
    public string File { get; set; }
    public long Entries { get; set; }
    public uint DocCount { get; set; }
    public uint UnitCount { get; set; }
    public long CurrentBytes { get; set; }
    public long Fixed8Bytes { get; set; }
    public long Blocked64Bytes { get; set; }
    public long Blocked256Bytes { get; set; }
    public double Fixed8EncodeMs { get; set; }
    public double Blocked64EncodeMs { get; set; }
    public double Blocked256EncodeMs { get; set; }
    public int ValidationKeys { get; set; }
}

public sealed class PrototypeBenchmarkResult {
    public int SchemaVersion { get; set; }
    public string IndexPath { get; set; }
    public string AnalysisMode { get; set; }
    public string Format { get; set; }
    public string DirectoryKind { get; set; }
    public long IndexBytes { get; set; }
    public double IndexGiB { get; set; }
    public int SegmentCount { get; set; }
    public long Entries { get; set; }
    public long CurrentCq3DirBytes { get; set; }
    public double CurrentCq3DirGiB { get; set; }
    public int QueriesPerWorkloadPerSegment { get; set; }
    public int Repeats { get; set; }
    public int BatchSize { get; set; }
    public long Seed { get; set; }
    public int CorrectnessValidationKeys { get; set; }
    public List<RepresentationSummary> Representations { get; set; }
    public List<AggregateTiming> Timings { get; set; }
    public List<SegmentSummary> Segments { get; set; }
    public string[] Workloads { get; set; }
    public string[] Caveats { get; set; }
}

internal static class ProtoCodec {
    internal const int HeaderSize = 512;
    internal const int SectionCount = 23;
    internal const int Cq3DirSection = 13;
    internal const int Cq3PostSection = 14;
    internal const int PrefixBytes = 257 * 4;
    internal const int CurrentEntryBytes = 10;

    internal static ushort U16(byte[] b, int o) {
        return (ushort)(b[o] | (b[o + 1] << 8));
    }

    internal static uint U32(byte[] b, int o) {
        return (uint)(b[o]
            | (b[o + 1] << 8)
            | (b[o + 2] << 16)
            | (b[o + 3] << 24));
    }

    internal static ulong U64(byte[] b, int o) {
        return (ulong)U32(b, o) | ((ulong)U32(b, o + 4) << 32);
    }

    internal static void PutU16(byte[] b, int o, ushort value) {
        b[o] = (byte)value;
        b[o + 1] = (byte)(value >> 8);
    }

    internal static void PutU32(byte[] b, int o, uint value) {
        b[o] = (byte)value;
        b[o + 1] = (byte)(value >> 8);
        b[o + 2] = (byte)(value >> 16);
        b[o + 3] = (byte)(value >> 24);
    }

    internal static int VarintBytes(uint value) {
        int size = 1;
        while (value >= 0x80) {
            value >>= 7;
            size++;
        }
        return size;
    }

    internal static int WriteVarint(byte[] output, int pos, uint value) {
        while (value >= 0x80) {
            output[pos++] = (byte)(value | 0x80);
            value >>= 7;
        }
        output[pos++] = (byte)value;
        return pos;
    }

    internal static uint ReadVarint(byte[] input, ref int pos, int end) {
        uint value = 0;
        int shift = 0;
        while (true) {
            if (pos >= end) throw new InvalidDataException("truncated prototype varint");
            byte b = input[pos++];
            if (shift > 28) throw new InvalidDataException("prototype varint overflow");
            value |= (uint)(b & 0x7f) << shift;
            if ((b & 0x80) == 0) return value;
            shift += 7;
        }
    }

    internal static void ReadExactly(Stream stream, byte[] buffer, int offset, int count) {
        int total = 0;
        while (total < count) {
            int n = stream.Read(buffer, offset + total, count - total);
            if (n <= 0) throw new EndOfStreamException("Unexpected end of PRSEG.");
            total += n;
        }
    }
}

public sealed class SegmentSource {
    public string File { get; private set; }
    public byte[] CurrentDirectory { get; private set; }
    public uint[] Prefix { get; private set; }
    public int Entries { get; private set; }
    public uint PostBytes { get; private set; }
    public uint DocCount { get; private set; }
    public uint UnitCount { get; private set; }

    private SegmentSource() {}

    public static SegmentSource Open(string path) {
        var info = new FileInfo(path);
        using (var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 4 * 1024 * 1024, FileOptions.SequentialScan)) {
            byte[] header = new byte[ProtoCodec.HeaderSize];
            ProtoCodec.ReadExactly(fs, header, 0, header.Length);

            string magic = System.Text.Encoding.ASCII.GetString(header, 0, 8);
            if (magic != "PRSEG005") throw new InvalidDataException(path + ": expected PRSEG005");
            if (ProtoCodec.U32(header, 8) != 5) throw new InvalidDataException(path + ": unexpected version");
            if (ProtoCodec.U32(header, 28) != ProtoCodec.SectionCount) throw new InvalidDataException(path + ": unexpected section count");
            if (ProtoCodec.U32(header, 480) != 2) throw new InvalidDataException(path + ": CQ3DIR is not Prefix10");

            int dirDesc = 32 + ProtoCodec.Cq3DirSection * 16;
            int postDesc = 32 + ProtoCodec.Cq3PostSection * 16;
            long dirOff = checked((long)ProtoCodec.U64(header, dirDesc));
            long dirSize = checked((long)ProtoCodec.U64(header, dirDesc + 8));
            long postOff = checked((long)ProtoCodec.U64(header, postDesc));
            long postSize = checked((long)ProtoCodec.U64(header, postDesc + 8));
            long payloadEnd = info.Length - 16;

            if (dirOff < ProtoCodec.HeaderSize || dirSize < ProtoCodec.PrefixBytes || dirOff + dirSize > payloadEnd)
                throw new InvalidDataException(path + ": CQ3DIR descriptor out of bounds");
            if (postOff < ProtoCodec.HeaderSize || postSize < 0 || postOff + postSize > payloadEnd)
                throw new InvalidDataException(path + ": CQ3POST descriptor out of bounds");
            if (postSize > UInt32.MaxValue) throw new InvalidDataException(path + ": CQ3POST exceeds u32 offsets");
            if ((dirSize - ProtoCodec.PrefixBytes) % ProtoCodec.CurrentEntryBytes != 0)
                throw new InvalidDataException(path + ": bad Prefix10 CQ3DIR size");
            if (dirSize > Int32.MaxValue) throw new InvalidDataException(path + ": CQ3DIR too large for prototype memory model");

            fs.Position = info.Length - 16;
            byte[] footer = new byte[8];
            ProtoCodec.ReadExactly(fs, footer, 0, footer.Length);
            if (System.Text.Encoding.ASCII.GetString(footer) != "PRFTR005")
                throw new InvalidDataException(path + ": bad footer magic");

            byte[] directory = new byte[(int)dirSize];
            fs.Position = dirOff;
            ProtoCodec.ReadExactly(fs, directory, 0, directory.Length);

            uint[] prefix = new uint[257];
            for (int i = 0; i < 257; i++) prefix[i] = ProtoCodec.U32(directory, i * 4);
            int entries = checked((int)((dirSize - ProtoCodec.PrefixBytes) / ProtoCodec.CurrentEntryBytes));
            if (prefix[0] != 0 || prefix[256] != (uint)entries)
                throw new InvalidDataException(path + ": prefix table does not span all entries");
            for (int p = 0; p < 256; p++) {
                if (prefix[p] > prefix[p + 1]) throw new InvalidDataException(path + ": non-monotonic prefix table");
            }

            uint previousOffset = 0;
            bool haveOffset = false;
            for (int p = 0; p < 256; p++) {
                ushort prevSuffix = 0;
                bool haveSuffix = false;
                for (int i = (int)prefix[p]; i < (int)prefix[p + 1]; i++) {
                    int o = ProtoCodec.PrefixBytes + i * ProtoCodec.CurrentEntryBytes;
                    ushort suffix = ProtoCodec.U16(directory, o);
                    uint packed = ProtoCodec.U32(directory, o + 2);
                    uint offset = ProtoCodec.U32(directory, o + 6);
                    uint count = packed & 0x3fffffffU;
                    if (count == 0) throw new InvalidDataException(path + ": zero CQ3 count");
                    if (offset > (uint)postSize) throw new InvalidDataException(path + ": CQ3 offset exceeds postings");
                    if (haveSuffix && suffix <= prevSuffix) throw new InvalidDataException(path + ": suffixes are not strictly increasing");
                    if (haveOffset && offset < previousOffset) throw new InvalidDataException(path + ": offsets are not monotonic");
                    prevSuffix = suffix;
                    haveSuffix = true;
                    previousOffset = offset;
                    haveOffset = true;
                }
            }

            return new SegmentSource {
                File = info.Name,
                CurrentDirectory = directory,
                Prefix = prefix,
                Entries = entries,
                PostBytes = (uint)postSize,
                DocCount = ProtoCodec.U32(header, 20),
                UnitCount = ProtoCodec.U32(header, 24)
            };
        }
    }

    public uint KeyAtEntry(int index) {
        if (index < 0 || index >= Entries) throw new ArgumentOutOfRangeException("index");
        int lo = 0;
        int hi = 256;
        uint uindex = (uint)index;
        while (lo + 1 < hi) {
            int mid = (lo + hi) / 2;
            if (Prefix[mid] <= uindex) lo = mid;
            else hi = mid;
        }
        int offset = ProtoCodec.PrefixBytes + index * ProtoCodec.CurrentEntryBytes;
        ushort suffix = ProtoCodec.U16(CurrentDirectory, offset);
        return ((uint)lo << 16) | suffix;
    }
}

public sealed class CurrentPrefix10Lookup : ICq3PrototypeLookup {
    private readonly byte[] data;
    private readonly uint[] prefix;
    private readonly int entries;
    private readonly uint postBytes;

    public CurrentPrefix10Lookup(SegmentSource source) {
        data = source.CurrentDirectory;
        prefix = source.Prefix;
        entries = source.Entries;
        postBytes = source.PostBytes;
    }

    public string Name { get { return "current-prefix10"; } }
    public long StorageBytes { get { return data.LongLength; } }

    public bool TryLookup(uint key, out ProtoMeta meta) {
        int p = (int)(key >> 16);
        int lo = (int)prefix[p];
        int end = (int)prefix[p + 1];
        ushort target = (ushort)key;
        int hi = end;
        while (lo < hi) {
            int mid = lo + ((hi - lo) >> 1);
            ushort value = ProtoCodec.U16(data, ProtoCodec.PrefixBytes + mid * 10);
            if (value < target) lo = mid + 1;
            else hi = mid;
        }
        if (lo == end || ProtoCodec.U16(data, ProtoCodec.PrefixBytes + lo * 10) != target) {
            meta = new ProtoMeta { Found = false };
            return false;
        }
        int o = ProtoCodec.PrefixBytes + lo * 10;
        uint packed = ProtoCodec.U32(data, o + 2);
        uint offset = ProtoCodec.U32(data, o + 6);
        uint nextOffset = lo + 1 < entries
            ? ProtoCodec.U32(data, ProtoCodec.PrefixBytes + (lo + 1) * 10 + 6)
            : postBytes;
        meta = new ProtoMeta {
            Found = true,
            Encoding = (int)(packed >> 30),
            Count = packed & 0x3fffffffU,
            Offset = offset,
            Bytes = nextOffset - offset
        };
        return true;
    }
}

public sealed class Fixed8Lookup : ICq3PrototypeLookup {
    private readonly byte[] data;
    private readonly uint[] prefix;
    private readonly int entries;
    private readonly uint postBytes;

    public Fixed8Lookup(SegmentSource source) {
        prefix = source.Prefix;
        entries = source.Entries;
        postBytes = source.PostBytes;
        data = new byte[checked(ProtoCodec.PrefixBytes + entries * 8)];
        for (int i = 0; i < 257; i++) ProtoCodec.PutU32(data, i * 4, prefix[i]);
        for (int i = 0; i < entries; i++) {
            int src = ProtoCodec.PrefixBytes + i * 10;
            ushort suffix = ProtoCodec.U16(source.CurrentDirectory, src);
            uint packed = ProtoCodec.U32(source.CurrentDirectory, src + 2);
            uint offset = ProtoCodec.U32(source.CurrentDirectory, src + 6);
            uint count = packed & 0x3fffffffU;
            uint enc = packed >> 30;
            if (count > 0x3fffU) throw new InvalidDataException(source.File + ": fixed8 requires count <= 16383");
            ushort packed16 = (ushort)((enc << 14) | count);
            int dst = ProtoCodec.PrefixBytes + i * 8;
            ProtoCodec.PutU16(data, dst, suffix);
            ProtoCodec.PutU16(data, dst + 2, packed16);
            ProtoCodec.PutU32(data, dst + 4, offset);
        }
    }

    public string Name { get { return "fixed8-packed14"; } }
    public long StorageBytes { get { return data.LongLength; } }

    public bool TryLookup(uint key, out ProtoMeta meta) {
        int p = (int)(key >> 16);
        int lo = (int)prefix[p];
        int end = (int)prefix[p + 1];
        ushort target = (ushort)key;
        int hi = end;
        while (lo < hi) {
            int mid = lo + ((hi - lo) >> 1);
            ushort value = ProtoCodec.U16(data, ProtoCodec.PrefixBytes + mid * 8);
            if (value < target) lo = mid + 1;
            else hi = mid;
        }
        if (lo == end || ProtoCodec.U16(data, ProtoCodec.PrefixBytes + lo * 8) != target) {
            meta = new ProtoMeta { Found = false };
            return false;
        }
        int o = ProtoCodec.PrefixBytes + lo * 8;
        ushort packed16 = ProtoCodec.U16(data, o + 2);
        uint offset = ProtoCodec.U32(data, o + 4);
        uint nextOffset = lo + 1 < entries
            ? ProtoCodec.U32(data, ProtoCodec.PrefixBytes + (lo + 1) * 8 + 4)
            : postBytes;
        meta = new ProtoMeta {
            Found = true,
            Encoding = packed16 >> 14,
            Count = (uint)(packed16 & 0x3fff),
            Offset = offset,
            Bytes = nextOffset - offset
        };
        return true;
    }
}

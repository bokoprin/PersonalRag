[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$IndexPath,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'

$resolvedIndex = [IO.Path]::GetFullPath($IndexPath).TrimEnd('\', '/')
if (-not (Test-Path -LiteralPath $resolvedIndex -PathType Container)) {
    throw "Index directory not found: $resolvedIndex"
}

$resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
if ($resolvedOutput.StartsWith($resolvedIndex + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
    $resolvedOutput.Equals($resolvedIndex, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputPath must be outside the index directory. Refusing to write into the index."
}

$outParent = Split-Path -Parent $resolvedOutput
if ([string]::IsNullOrWhiteSpace($outParent)) {
    $outParent = (Get-Location).Path
}
[IO.Directory]::CreateDirectory($outParent) | Out-Null

$csharp = @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;

public sealed class Cq3Candidate {
    public string Name { get; set; }
    public string LookupModel { get; set; }
    public long? DirectoryBytes { get; set; }
    public double? DirectoryGiB { get; set; }
    public double? DirectoryReductionPercent { get; set; }
    public long? WholeIndexBytes { get; set; }
    public double? WholeIndexGiB { get; set; }
    public double? WholeIndexReductionPercent { get; set; }
    public bool Applicable { get; set; }
    public string Note { get; set; }
}

public sealed class Cq3Segment {
    public string File { get; set; }
    public long FileBytes { get; set; }
    public uint DocCount { get; set; }
    public uint UnitCount { get; set; }
    public long Cq3DirOffset { get; set; }
    public long Cq3DirBytes { get; set; }
    public long Cq3PostBytes { get; set; }
    public long Entries { get; set; }
    public long[] PrefixEntryCounts { get; set; }
    public long[] EncodingCounts { get; set; }
    public long[] CountBitWidthHistogram { get; set; }
    public long[] PackedTokenVarintBytesHistogram { get; set; }
    public long[] KeyGapVarintBytesHistogram { get; set; }
    public long[] OffsetTokenVarintBytesHistogram { get; set; }
    public uint MaxCount { get; set; }
    public uint MaxOffset { get; set; }
    public int CountBits { get; set; }
    public int PackedBits { get; set; }
    public int OffsetBits { get; set; }
    public int FixedPackedBytes { get; set; }
    public int FixedOffsetBytes { get; set; }
    public bool FitsPacked14 { get; set; }
    public bool FitsAbsoluteOffset24 { get; set; }
    public long CurrentBytes { get; set; }
    public long Fixed8Bytes { get; set; }
    public long FixedMinimalByteFieldsBytes { get; set; }
    public long FixedBitPackedBytes { get; set; }
    public long DeltaVarintStreamBytes { get; set; }
    public long BitmapRankU32Bytes { get; set; }
    public long BitmapRankPacked16Bytes { get; set; }
    public long BitmapRankBitPackedBytes { get; set; }
    // PowerShell 7's ConvertTo-Json rejects dictionaries with non-string keys.
    // Keep the modeled block sizes identical, but expose them with JSON-safe keys.
    public Dictionary<string, long> BlockedDeltaBytes { get; set; }
    public Dictionary<string, long> BlockCounts { get; set; }
}

public sealed class Cq3Analysis {
    public int SchemaVersion { get; set; }
    public string IndexPath { get; set; }
    public string AnalysisMode { get; set; }
    public string Format { get; set; }
    public string DirectoryKind { get; set; }
    public long IndexBytes { get; set; }
    public double IndexGiB { get; set; }
    public int SegmentCount { get; set; }
    public long Cq3DirBytes { get; set; }
    public double Cq3DirGiB { get; set; }
    public double Cq3DirPercentOfIndex { get; set; }
    public long Entries { get; set; }
    public double AverageBytesPerEntry { get; set; }
    public uint MaxCount { get; set; }
    public uint MaxOffset { get; set; }
    public bool AllSegmentsFitPacked14 { get; set; }
    public long[] EncodingCounts { get; set; }
    public string[] EncodingNames { get; set; }
    public long[] CountBitWidthHistogram { get; set; }
    public long[] PackedTokenVarintBytesHistogram { get; set; }
    public long[] KeyGapVarintBytesHistogram { get; set; }
    public long[] OffsetTokenVarintBytesHistogram { get; set; }
    public List<Cq3Segment> Segments { get; set; }
    public List<Cq3Candidate> Candidates { get; set; }
    public string[] Caveats { get; set; }
}

public static class Cq3ReadOnlyAnalyzer {
    private const int HeaderSize = 512;
    private const int SectionCount = 23;
    private const int Cq3DirSection = 13;
    private const int Cq3PostSection = 14;
    private const int PrefixBytes = 257 * 4;
    private const int CurrentEntryBytes = 10;
    private const int BitmapBytes = (1 << 24) / 8;
    private const int RankStrideBits = 512;
    private const int RankEntries = (1 << 24) / RankStrideBits + 1;
    private const int RankBytes = RankEntries * 4;
    private static readonly int[] BlockSizes = new [] { 16, 32, 64, 128, 256 };

    private static ushort U16(byte[] b, int o) {
        return (ushort)(b[o] | (b[o + 1] << 8));
    }

    private static uint U32(byte[] b, int o) {
        return (uint)(b[o]
            | (b[o + 1] << 8)
            | (b[o + 2] << 16)
            | (b[o + 3] << 24));
    }

    private static ulong U64(byte[] b, int o) {
        return (ulong)U32(b, o) | ((ulong)U32(b, o + 4) << 32);
    }

    private static int BitsRequired(ulong value) {
        if (value == 0) return 1;
        int bits = 0;
        while (value != 0) {
            bits++;
            value >>= 1;
        }
        return bits;
    }

    private static int VarintBytes(ulong value) {
        int n = 1;
        while (value >= 0x80) {
            value >>= 7;
            n++;
        }
        return n;
    }

    private static long CeilBitsToBytes(long entries, int bitsPerEntry) {
        checked {
            long bits = entries * (long)bitsPerEntry;
            return (bits + 7) / 8;
        }
    }

    private static void ReadExactly(Stream stream, byte[] buffer, int offset, int count) {
        int readTotal = 0;
        while (readTotal < count) {
            int n = stream.Read(buffer, offset + readTotal, count - readTotal);
            if (n <= 0) throw new EndOfStreamException("Unexpected end of CQ3DIR.");
            readTotal += n;
        }
    }

    private static long SumIndexBytes(string indexPath) {
        long total = 0;
        foreach (var path in Directory.EnumerateFiles(indexPath, "*", SearchOption.AllDirectories)) {
            checked { total += new FileInfo(path).Length; }
        }
        return total;
    }

    private static Cq3Segment AnalyzeSegment(string path) {
        var info = new FileInfo(path);
        using (var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite, 4 * 1024 * 1024, FileOptions.SequentialScan)) {
            byte[] header = new byte[HeaderSize];
            ReadExactly(fs, header, 0, header.Length);

            string magic = System.Text.Encoding.ASCII.GetString(header, 0, 8);
            if (magic != "PRSEG005") throw new InvalidDataException(path + ": expected PRSEG005, got " + magic);
            if (U32(header, 8) != 5) throw new InvalidDataException(path + ": unexpected segment version");
            if (U32(header, 28) != SectionCount) throw new InvalidDataException(path + ": unexpected section count");
            if (U32(header, 480) != 2) throw new InvalidDataException(path + ": CQ3DIR is not Prefix10");

            uint docCount = U32(header, 20);
            uint unitCount = U32(header, 24);

            int dirDesc = 32 + Cq3DirSection * 16;
            int postDesc = 32 + Cq3PostSection * 16;
            long dirOff = checked((long)U64(header, dirDesc));
            long dirSize = checked((long)U64(header, dirDesc + 8));
            long postOff = checked((long)U64(header, postDesc));
            long postSize = checked((long)U64(header, postDesc + 8));

            long payloadEnd = info.Length - 16;
            if (dirOff < HeaderSize || dirSize < PrefixBytes || dirOff + dirSize > payloadEnd)
                throw new InvalidDataException(path + ": CQ3DIR descriptor out of bounds");
            if (postOff < HeaderSize || postSize < 0 || postOff + postSize > payloadEnd)
                throw new InvalidDataException(path + ": CQ3POST descriptor out of bounds");
            if ((dirSize - PrefixBytes) % CurrentEntryBytes != 0)
                throw new InvalidDataException(path + ": CQ3DIR size is not prefix + N*10");

            fs.Position = info.Length - 16;
            byte[] footer = new byte[8];
            ReadExactly(fs, footer, 0, footer.Length);
            if (System.Text.Encoding.ASCII.GetString(footer) != "PRFTR005")
                throw new InvalidDataException(path + ": bad footer magic");

            long entries = (dirSize - PrefixBytes) / CurrentEntryBytes;
            if (entries > Int32.MaxValue)
                throw new InvalidDataException(path + ": entry count exceeds analyzer prefix-index limit");

            fs.Position = dirOff;
            byte[] prefixRaw = new byte[PrefixBytes];
            ReadExactly(fs, prefixRaw, 0, prefixRaw.Length);
            uint[] prefix = new uint[257];
            long[] prefixCounts = new long[256];
            for (int i = 0; i < 257; i++) prefix[i] = U32(prefixRaw, i * 4);
            if (prefix[0] != 0 || prefix[256] != (uint)entries)
                throw new InvalidDataException(path + ": prefix table does not span all entries");
            for (int i = 0; i < 256; i++) {
                if (prefix[i] > prefix[i + 1]) throw new InvalidDataException(path + ": prefix table is not monotonic");
                prefixCounts[i] = prefix[i + 1] - prefix[i];
            }

            long[] encCounts = new long[4];
            long[] countBitsHist = new long[31];
            long[] packedVarintHist = new long[6];
            long[] keyGapVarintHist = new long[4];
            long[] offsetVarintHist = new long[6];

            uint maxCount = 0;
            uint maxOffset = 0;
            uint previousGlobalOffset = 0;
            bool havePreviousGlobalOffset = false;

            long deltaStreamPayloadBytes = 0;
            long[] blockedPayload = new long[BlockSizes.Length];
            long[] blockCounts = new long[BlockSizes.Length];

            int high = 0;
            long indexInPrefix = 0;
            ushort previousSuffix = 0;
            uint previousOffsetInPrefix = 0;

            long[] blockPosition = new long[BlockSizes.Length];
            ushort[] blockPreviousSuffix = new ushort[BlockSizes.Length];
            uint[] blockPreviousOffset = new uint[BlockSizes.Length];

            fs.Position = dirOff + PrefixBytes;
            const int RecordsPerChunk = 131072;
            byte[] chunk = new byte[RecordsPerChunk * CurrentEntryBytes];
            long remaining = entries;
            long absoluteIndex = 0;

            while (remaining > 0) {
                int take = (int)Math.Min((long)RecordsPerChunk, remaining);
                int bytes = take * CurrentEntryBytes;
                ReadExactly(fs, chunk, 0, bytes);

                for (int r = 0; r < take; r++, absoluteIndex++) {
                    while (high < 255 && absoluteIndex >= prefix[high + 1]) {
                        high++;
                        indexInPrefix = 0;
                        previousSuffix = 0;
                        previousOffsetInPrefix = 0;
                        for (int bi = 0; bi < BlockSizes.Length; bi++) {
                            blockPosition[bi] = 0;
                            blockPreviousSuffix[bi] = 0;
                            blockPreviousOffset[bi] = 0;
                        }
                    }

                    int o = r * CurrentEntryBytes;
                    ushort suffix = U16(chunk, o);
                    uint packed = U32(chunk, o + 2);
                    uint offset = U32(chunk, o + 6);
                    int encodingIndex = (int)(packed >> 30);
                    uint count = packed & 0x3fffffffU;
                    if (encodingIndex < 0 || encodingIndex > 3)
                        throw new InvalidDataException(path + ": invalid encoding");
                    if (count == 0)
                        throw new InvalidDataException(path + ": zero posting count in CQ3DIR");
                    if ((ulong)offset > (ulong)postSize)
                        throw new InvalidDataException(path + ": posting offset exceeds CQ3POST");

                    if (indexInPrefix > 0 && suffix <= previousSuffix)
                        throw new InvalidDataException(path + ": suffixes are not strictly increasing within prefix");
                    if (havePreviousGlobalOffset && offset < previousGlobalOffset)
                        throw new InvalidDataException(path + ": CQ3POST offsets are not monotonic");

                    encCounts[encodingIndex]++;
                    if (count > maxCount) maxCount = count;
                    if (offset > maxOffset) maxOffset = offset;

                    int cb = BitsRequired(count);
                    countBitsHist[Math.Min(cb, countBitsHist.Length - 1)]++;

                    ulong packedToken = ((ulong)count << 2) | (uint)encodingIndex;
                    int pv = VarintBytes(packedToken);
                    packedVarintHist[Math.Min(pv, packedVarintHist.Length - 1)]++;

                    ulong keyGap = indexInPrefix == 0 ? suffix : (ulong)(suffix - previousSuffix);
                    int kgv = VarintBytes(keyGap);
                    keyGapVarintHist[Math.Min(kgv, keyGapVarintHist.Length - 1)]++;

                    ulong offsetToken = indexInPrefix == 0 ? offset : (ulong)(offset - previousOffsetInPrefix);
                    int odv = VarintBytes(offsetToken);
                    offsetVarintHist[Math.Min(odv, offsetVarintHist.Length - 1)]++;

                    checked {
                        deltaStreamPayloadBytes += kgv + pv + odv;
                    }

                    for (int bi = 0; bi < BlockSizes.Length; bi++) {
                        int blockSize = BlockSizes[bi];
                        bool firstInBlock = blockPosition[bi] == 0;
                        if (firstInBlock) {
                            blockCounts[bi]++;
                            checked { blockedPayload[bi] += pv; }
                        } else {
                            ulong blockKeyGap = (ulong)(suffix - blockPreviousSuffix[bi]);
                            ulong blockOffsetDelta = (ulong)(offset - blockPreviousOffset[bi]);
                            checked {
                                blockedPayload[bi] += VarintBytes(blockKeyGap) + pv + VarintBytes(blockOffsetDelta);
                            }
                        }
                        blockPreviousSuffix[bi] = suffix;
                        blockPreviousOffset[bi] = offset;
                        blockPosition[bi]++;
                        if (blockPosition[bi] == blockSize) blockPosition[bi] = 0;
                    }

                    previousSuffix = suffix;
                    previousOffsetInPrefix = offset;
                    previousGlobalOffset = offset;
                    havePreviousGlobalOffset = true;
                    indexInPrefix++;
                }

                remaining -= take;
            }

            int countBits = BitsRequired(maxCount);
            int packedBits = countBits + 2;
            int offsetBits = BitsRequired(maxOffset);
            int fixedPackedBytes = (packedBits + 7) / 8;
            int fixedOffsetBytes = (offsetBits + 7) / 8;
            bool fitsPacked14 = maxCount <= 0x3fffU;
            bool fitsOffset24 = maxOffset <= 0x00ffffffU;

            long current = dirSize;
            long fixed8 = fitsPacked14 ? checked((long)PrefixBytes + entries * 8L) : -1L;
            long fixedMinimal = checked((long)PrefixBytes + entries * (2L + fixedPackedBytes + fixedOffsetBytes));
            long fixedBitPacked = checked((long)PrefixBytes + CeilBitsToBytes(entries, 16 + packedBits + offsetBits));
            long deltaStream = checked((long)PrefixBytes + deltaStreamPayloadBytes);

            long bitmapBase = BitmapBytes + RankBytes;
            long bitmapU32 = checked(bitmapBase + entries * 8L);
            long bitmapPacked16 = fitsPacked14 ? checked(bitmapBase + entries * 6L) : -1L;
            long bitmapBitPacked = checked(bitmapBase + CeilBitsToBytes(entries, packedBits + offsetBits));

            var blocked = new Dictionary<string, long>();
            var blocks = new Dictionary<string, long>();
            for (int bi = 0; bi < BlockSizes.Length; bi++) {
                long size = checked((long)PrefixBytes + blockCounts[bi] * 10L + blockedPayload[bi]);
                string blockKey = BlockSizes[bi].ToString(System.Globalization.CultureInfo.InvariantCulture);
                blocked[blockKey] = size;
                blocks[blockKey] = blockCounts[bi];
            }

            return new Cq3Segment {
                File = info.Name,
                FileBytes = info.Length,
                DocCount = docCount,
                UnitCount = unitCount,
                Cq3DirOffset = dirOff,
                Cq3DirBytes = dirSize,
                Cq3PostBytes = postSize,
                Entries = entries,
                PrefixEntryCounts = prefixCounts,
                EncodingCounts = encCounts,
                CountBitWidthHistogram = countBitsHist,
                PackedTokenVarintBytesHistogram = packedVarintHist,
                KeyGapVarintBytesHistogram = keyGapVarintHist,
                OffsetTokenVarintBytesHistogram = offsetVarintHist,
                MaxCount = maxCount,
                MaxOffset = maxOffset,
                CountBits = countBits,
                PackedBits = packedBits,
                OffsetBits = offsetBits,
                FixedPackedBytes = fixedPackedBytes,
                FixedOffsetBytes = fixedOffsetBytes,
                FitsPacked14 = fitsPacked14,
                FitsAbsoluteOffset24 = fitsOffset24,
                CurrentBytes = current,
                Fixed8Bytes = fixed8,
                FixedMinimalByteFieldsBytes = fixedMinimal,
                FixedBitPackedBytes = fixedBitPacked,
                DeltaVarintStreamBytes = deltaStream,
                BitmapRankU32Bytes = bitmapU32,
                BitmapRankPacked16Bytes = bitmapPacked16,
                BitmapRankBitPackedBytes = bitmapBitPacked,
                BlockedDeltaBytes = blocked,
                BlockCounts = blocks
            };
        }
    }

    private static long SumOrNegative(IEnumerable<long> values) {
        long sum = 0;
        foreach (var value in values) {
            if (value < 0) return -1;
            checked { sum += value; }
        }
        return sum;
    }

    private static Cq3Candidate MakeCandidate(
        string name, string lookupModel, long bytes, long currentDir, long indexBytes, string note) {
        bool applicable = bytes >= 0;
        return new Cq3Candidate {
            Name = name,
            LookupModel = lookupModel,
            DirectoryBytes = applicable ? (long?)bytes : null,
            DirectoryGiB = applicable ? (double?)((double)bytes / (1024.0 * 1024.0 * 1024.0)) : null,
            DirectoryReductionPercent = applicable ? (double?)(((double)currentDir - bytes) / currentDir * 100.0) : null,
            WholeIndexBytes = applicable ? (long?)(indexBytes - currentDir + bytes) : null,
            WholeIndexGiB = applicable ? (double?)((double)(indexBytes - currentDir + bytes) / (1024.0 * 1024.0 * 1024.0)) : null,
            WholeIndexReductionPercent = applicable ? (double?)(((double)currentDir - bytes) / indexBytes * 100.0) : null,
            Applicable = applicable,
            Note = note
        };
    }

    private static void AddHist(long[] target, long[] source) {
        for (int i = 0; i < Math.Min(target.Length, source.Length); i++) target[i] += source[i];
    }

    public static Cq3Analysis Analyze(string indexPath) {
        indexPath = Path.GetFullPath(indexPath);
        var files = Directory.GetFiles(indexPath, "seg-*.prseg", SearchOption.TopDirectoryOnly)
            .OrderBy(p => p, StringComparer.OrdinalIgnoreCase)
            .ToArray();
        if (files.Length == 0) throw new InvalidDataException("No seg-*.prseg files found.");

        long indexBytes = SumIndexBytes(indexPath);
        var segments = new List<Cq3Segment>();
        foreach (var file in files) segments.Add(AnalyzeSegment(file));

        long currentDir = segments.Sum(s => s.Cq3DirBytes);
        long entries = segments.Sum(s => s.Entries);
        uint maxCount = segments.Max(s => s.MaxCount);
        uint maxOffset = segments.Max(s => s.MaxOffset);
        bool allPacked14 = segments.All(s => s.FitsPacked14);

        long[] enc = new long[4];
        long[] countBits = new long[31];
        long[] packedVarint = new long[6];
        long[] keyGapVarint = new long[4];
        long[] offsetVarint = new long[6];
        foreach (var s in segments) {
            AddHist(enc, s.EncodingCounts);
            AddHist(countBits, s.CountBitWidthHistogram);
            AddHist(packedVarint, s.PackedTokenVarintBytesHistogram);
            AddHist(keyGapVarint, s.KeyGapVarintBytesHistogram);
            AddHist(offsetVarint, s.OffsetTokenVarintBytesHistogram);
        }

        var candidates = new List<Cq3Candidate>();
        candidates.Add(MakeCandidate(
            "current-prefix10",
            "prefix table + binary search over fixed 10-byte records",
            currentDir, currentDir, indexBytes,
            "Current PRSEG005 representation: suffix u16 + packed(count,encoding) u32 + payload offset u32."
        ));

        long fixed8 = SumOrNegative(segments.Select(s => s.Fixed8Bytes));
        candidates.Add(MakeCandidate(
            "fixed8-packed14",
            "same prefix partition; O(1) fixed-record addressing and binary search",
            fixed8, currentDir, indexBytes,
            "Uses suffix u16 + packed14(count)+2-bit encoding in u16 + offset u32. Applicable only if every count <= 16383."
        ));

        long fixedMinimal = segments.Sum(s => s.FixedMinimalByteFieldsBytes);
        candidates.Add(MakeCandidate(
            "fixed-minimal-byte-fields",
            "same prefix partition; fixed-width records, but segment-specific nonstandard field widths",
            fixedMinimal, currentDir, indexBytes,
            "Theoretical byte-aligned lower bound using the minimum whole-byte width required by each segment's max count and max absolute offset."
        ));

        long fixedBitPacked = segments.Sum(s => s.FixedBitPackedBytes);
        candidates.Add(MakeCandidate(
            "fixed-bit-packed",
            "same prefix partition; bit-addressable fixed records",
            fixedBitPacked, currentDir, indexBytes,
            "Theoretical bit-packed fixed-width lower bound using segment-specific packed-count and absolute-offset bit widths. Random lookup remains indexable but decode complexity increases."
        ));

        long deltaStream = segments.Sum(s => s.DeltaVarintStreamBytes);
        candidates.Add(MakeCandidate(
            "prefix-delta-varint-stream",
            "prefix selects byte range, then sequential varint decode within prefix",
            deltaStream, currentDir, indexBytes,
            "Suffix gap, packed(count+encoding), and payload-offset delta use unsigned varints. First suffix/offset in each prefix are absolute."
        ));

        foreach (int blockSize in BlockSizes) {
            long bytes = segments.Sum(s => s.BlockedDeltaBytes[blockSize.ToString(System.Globalization.CultureInfo.InvariantCulture)]);
            candidates.Add(MakeCandidate(
                "blocked-delta-" + blockSize,
                "prefix -> binary-search 10-byte block checkpoints -> decode at most " + blockSize + " records",
                bytes, currentDir, indexBytes,
                "Each block checkpoint stores firstSuffix u16, streamOffset u32, basePayloadOffset u32; records use varint deltas."
            ));
        }

        long bitmapU32 = segments.Sum(s => s.BitmapRankU32Bytes);
        candidates.Add(MakeCandidate(
            "bitmap-rank-u32-values",
            "24-bit presence bitmap + rank512 + direct packed u32/offset u32 value arrays",
            bitmapU32, currentDir, indexBytes,
            "Per segment: 2 MiB key bitmap + 32-bit rank checkpoint every 512 keys + 8 bytes per present key."
        ));

        long bitmapPacked16 = SumOrNegative(segments.Select(s => s.BitmapRankPacked16Bytes));
        candidates.Add(MakeCandidate(
            "bitmap-rank-packed14",
            "24-bit presence bitmap + rank512 + direct packed u16/offset u32 value arrays",
            bitmapPacked16, currentDir, indexBytes,
            "Per segment: 2 MiB key bitmap + rank checkpoints + 6 bytes per present key. Requires all counts <= 16383."
        ));

        long bitmapBitPacked = segments.Sum(s => s.BitmapRankBitPackedBytes);
        candidates.Add(MakeCandidate(
            "bitmap-rank-bit-packed-values",
            "24-bit presence bitmap + rank512 + segment-specific bit-packed values",
            bitmapBitPacked, currentDir, indexBytes,
            "Theoretical compact random-lookup model: bitmap/rank locates ordinal; packed count+encoding and absolute offset use minimum segment-specific bit widths."
        ));

        return new Cq3Analysis {
            SchemaVersion = 1,
            IndexPath = indexPath,
            AnalysisMode = "read-only; opens PRSEG files FileAccess.Read and writes only OutputPath outside the index",
            Format = "PRSEG005",
            DirectoryKind = "Prefix10",
            IndexBytes = indexBytes,
            IndexGiB = (double)indexBytes / (1024.0 * 1024.0 * 1024.0),
            SegmentCount = segments.Count,
            Cq3DirBytes = currentDir,
            Cq3DirGiB = (double)currentDir / (1024.0 * 1024.0 * 1024.0),
            Cq3DirPercentOfIndex = (double)currentDir / indexBytes * 100.0,
            Entries = entries,
            AverageBytesPerEntry = (double)currentDir / entries,
            MaxCount = maxCount,
            MaxOffset = maxOffset,
            AllSegmentsFitPacked14 = allPacked14,
            EncodingCounts = enc,
            EncodingNames = new [] { "InlineU32", "DeltaVarint", "Block256Bitmap", "DenseBitset" },
            CountBitWidthHistogram = countBits,
            PackedTokenVarintBytesHistogram = packedVarint,
            KeyGapVarintBytesHistogram = keyGapVarint,
            OffsetTokenVarintBytesHistogram = offsetVarint,
            Segments = segments,
            Candidates = candidates,
            Caveats = new [] {
                "This tool estimates directory representation size only. It does not modify CQ3DIR/CQ3POST or benchmark search latency.",
                "Theoretical compact formats are not format-compatible with PRSEG005; they are sizing models for a future format study.",
                "Variable-length models trade random-access simplicity for decode work; size alone is not an ACCEPT criterion.",
                "The analyzer validates magic/version/section descriptors/prefix shape/order/offset monotonicity but intentionally does not recompute full PRSEG checksums."
            }
        };
    }
}
'@

Add-Type -TypeDefinition $csharp -Language CSharp

$result = [Cq3ReadOnlyAnalyzer]::Analyze($resolvedIndex)
$json = $result | ConvertTo-Json -Depth 20
[IO.File]::WriteAllText($resolvedOutput, $json, (New-Object Text.UTF8Encoding($false)))

$best = @($result.Candidates |
    Where-Object { $_.Applicable -and $_.Name -ne 'current-prefix10' } |
    Sort-Object DirectoryBytes |
    Select-Object -First 1)

Write-Host 'CQ3DIR_ANALYSIS_COMPLETE'
Write-Host "indexBytes=$($result.IndexBytes)"
Write-Host ("indexGiB={0:N6}" -f $result.IndexGiB)
Write-Host "segmentCount=$($result.SegmentCount)"
Write-Host "entries=$($result.Entries)"
Write-Host "cq3DirBytes=$($result.Cq3DirBytes)"
Write-Host ("cq3DirGiB={0:N6}" -f $result.Cq3DirGiB)
Write-Host ("cq3DirPercentOfIndex={0:N3}" -f $result.Cq3DirPercentOfIndex)
Write-Host "maxCount=$($result.MaxCount)"
Write-Host "allSegmentsFitPacked14=$($result.AllSegmentsFitPacked14)"
if ($best.Count -eq 1) {
    Write-Host "smallestModeledCandidate=$($best[0].Name)"
    Write-Host "smallestModeledDirectoryBytes=$($best[0].DirectoryBytes)"
    Write-Host ("smallestModeledDirectoryReductionPercent={0:N3}" -f $best[0].DirectoryReductionPercent)
    Write-Host ("smallestModeledWholeIndexGiB={0:N6}" -f $best[0].WholeIndexGiB)
}
Write-Host "output=$resolvedOutput"

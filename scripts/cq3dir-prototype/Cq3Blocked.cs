public sealed class BlockedDeltaLookup : ICq3PrototypeLookup {
    private readonly int blockSize;
    private readonly uint[] prefixBlocks;
    private readonly byte[] checkpoints;
    private readonly byte[] stream;
    private readonly uint postBytes;
    private readonly int checkpointCount;
    private readonly string name;

    public BlockedDeltaLookup(SegmentSource source, int blockSize) {
        if (blockSize != 64 && blockSize != 256) throw new ArgumentOutOfRangeException("blockSize");
        this.blockSize = blockSize;
        this.name = "blocked-delta-" + blockSize;
        this.postBytes = source.PostBytes;
        this.prefixBlocks = new uint[257];

        long totalBlocks = 0;
        long streamBytes = 0;
        for (int p = 0; p < 256; p++) {
            prefixBlocks[p] = checked((uint)totalBlocks);
            int start = (int)source.Prefix[p];
            int end = (int)source.Prefix[p + 1];
            int count = end - start;
            if (count == 0) continue;
            totalBlocks += (count + blockSize - 1) / blockSize;

            int i = start;
            while (i < end) {
                int blockEnd = Math.Min(end, i + blockSize);
                int first = ProtoCodec.PrefixBytes + i * 10;
                uint packed = ProtoCodec.U32(source.CurrentDirectory, first + 2);
                uint countValue = packed & 0x3fffffffU;
                uint enc = packed >> 30;
                uint token = checked((countValue << 2) | enc);
                streamBytes += ProtoCodec.VarintBytes(token);

                ushort prevSuffix = ProtoCodec.U16(source.CurrentDirectory, first);
                uint prevOffset = ProtoCodec.U32(source.CurrentDirectory, first + 6);
                for (int j = i + 1; j < blockEnd; j++) {
                    int o = ProtoCodec.PrefixBytes + j * 10;
                    ushort suffix = ProtoCodec.U16(source.CurrentDirectory, o);
                    uint pck = ProtoCodec.U32(source.CurrentDirectory, o + 2);
                    uint offset = ProtoCodec.U32(source.CurrentDirectory, o + 6);
                    uint cnt = pck & 0x3fffffffU;
                    uint en = pck >> 30;
                    uint tok = checked((cnt << 2) | en);
                    streamBytes += ProtoCodec.VarintBytes((uint)(suffix - prevSuffix));
                    streamBytes += ProtoCodec.VarintBytes(tok);
                    streamBytes += ProtoCodec.VarintBytes(offset - prevOffset);
                    prevSuffix = suffix;
                    prevOffset = offset;
                }
                i = blockEnd;
            }
        }
        prefixBlocks[256] = checked((uint)totalBlocks);
        if (totalBlocks > Int32.MaxValue) throw new InvalidDataException(source.File + ": too many prototype checkpoints");
        if (streamBytes > Int32.MaxValue) throw new InvalidDataException(source.File + ": prototype stream too large");
        checkpointCount = (int)totalBlocks;
        checkpoints = new byte[checked(checkpointCount * 10)];
        stream = new byte[(int)streamBytes];

        int cpIndex = 0;
        int streamPos = 0;
        for (int p = 0; p < 256; p++) {
            int start = (int)source.Prefix[p];
            int end = (int)source.Prefix[p + 1];
            int i = start;
            while (i < end) {
                int blockEnd = Math.Min(end, i + blockSize);
                int first = ProtoCodec.PrefixBytes + i * 10;
                ushort firstSuffix = ProtoCodec.U16(source.CurrentDirectory, first);
                uint firstPacked = ProtoCodec.U32(source.CurrentDirectory, first + 2);
                uint firstOffset = ProtoCodec.U32(source.CurrentDirectory, first + 6);
                int cp = cpIndex * 10;
                ProtoCodec.PutU16(checkpoints, cp, firstSuffix);
                ProtoCodec.PutU32(checkpoints, cp + 2, checked((uint)streamPos));
                ProtoCodec.PutU32(checkpoints, cp + 6, firstOffset);
                cpIndex++;

                uint firstToken = checked(((firstPacked & 0x3fffffffU) << 2) | (firstPacked >> 30));
                streamPos = ProtoCodec.WriteVarint(stream, streamPos, firstToken);

                ushort prevSuffix = firstSuffix;
                uint prevOffset = firstOffset;
                for (int j = i + 1; j < blockEnd; j++) {
                    int o = ProtoCodec.PrefixBytes + j * 10;
                    ushort suffix = ProtoCodec.U16(source.CurrentDirectory, o);
                    uint packed = ProtoCodec.U32(source.CurrentDirectory, o + 2);
                    uint offset = ProtoCodec.U32(source.CurrentDirectory, o + 6);
                    uint token = checked(((packed & 0x3fffffffU) << 2) | (packed >> 30));
                    streamPos = ProtoCodec.WriteVarint(stream, streamPos, (uint)(suffix - prevSuffix));
                    streamPos = ProtoCodec.WriteVarint(stream, streamPos, token);
                    streamPos = ProtoCodec.WriteVarint(stream, streamPos, offset - prevOffset);
                    prevSuffix = suffix;
                    prevOffset = offset;
                }
                i = blockEnd;
            }
        }
        if (cpIndex != checkpointCount || streamPos != stream.Length)
            throw new InvalidDataException(source.File + ": blocked prototype encode size mismatch");
    }

    public string Name { get { return name; } }
    public long StorageBytes { get { return ProtoCodec.PrefixBytes + checkpoints.LongLength + stream.LongLength; } }

    private ushort CheckpointSuffix(int index) {
        return ProtoCodec.U16(checkpoints, index * 10);
    }

    private uint CheckpointStream(int index) {
        return ProtoCodec.U32(checkpoints, index * 10 + 2);
    }

    private uint CheckpointOffset(int index) {
        return ProtoCodec.U32(checkpoints, index * 10 + 6);
    }

    public bool TryLookup(uint key, out ProtoMeta meta) {
        int p = (int)(key >> 16);
        int beginBlock = (int)prefixBlocks[p];
        int endBlock = (int)prefixBlocks[p + 1];
        ushort target = (ushort)key;
        if (beginBlock == endBlock) {
            meta = new ProtoMeta { Found = false };
            return false;
        }

        int lo = beginBlock;
        int hi = endBlock;
        while (lo < hi) {
            int mid = lo + ((hi - lo) >> 1);
            if (CheckpointSuffix(mid) <= target) lo = mid + 1;
            else hi = mid;
        }
        if (lo == beginBlock) {
            meta = new ProtoMeta { Found = false };
            return false;
        }
        int block = lo - 1;
        int pos = checked((int)CheckpointStream(block));
        int streamEnd = block + 1 < checkpointCount
            ? checked((int)CheckpointStream(block + 1))
            : stream.Length;
        ushort suffix = CheckpointSuffix(block);
        uint offset = CheckpointOffset(block);
        uint token = ProtoCodec.ReadVarint(stream, ref pos, streamEnd);

        while (true) {
            if (suffix == target) {
                uint nextOffset;
                if (pos < streamEnd) {
                    int tmp = pos;
                    uint gap = ProtoCodec.ReadVarint(stream, ref tmp, streamEnd);
                    ProtoCodec.ReadVarint(stream, ref tmp, streamEnd);
                    uint offsetDelta = ProtoCodec.ReadVarint(stream, ref tmp, streamEnd);
                    if (gap == 0) throw new InvalidDataException("zero suffix delta in prototype");
                    nextOffset = checked(offset + offsetDelta);
                } else if (block + 1 < checkpointCount) {
                    nextOffset = CheckpointOffset(block + 1);
                } else {
                    nextOffset = postBytes;
                }
                if (nextOffset < offset) throw new InvalidDataException("prototype next offset regression");
                meta = new ProtoMeta {
                    Found = true,
                    Encoding = (int)(token & 3U),
                    Count = token >> 2,
                    Offset = offset,
                    Bytes = nextOffset - offset
                };
                return true;
            }
            if (suffix > target || pos >= streamEnd) {
                meta = new ProtoMeta { Found = false };
                return false;
            }

            uint suffixDelta = ProtoCodec.ReadVarint(stream, ref pos, streamEnd);
            token = ProtoCodec.ReadVarint(stream, ref pos, streamEnd);
            uint offsetDelta2 = ProtoCodec.ReadVarint(stream, ref pos, streamEnd);
            if (suffixDelta == 0) throw new InvalidDataException("zero suffix delta in prototype");
            suffix = checked((ushort)(suffix + suffixDelta));
            offset = checked(offset + offsetDelta2);
        }
    }
}

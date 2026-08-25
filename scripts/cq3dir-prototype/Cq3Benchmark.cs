internal sealed class DeterministicRng {
    private ulong state;

    internal DeterministicRng(ulong seed) {
        state = seed == 0 ? 0x9e3779b97f4a7c15UL : seed;
    }

    internal ulong NextU64() {
        ulong x = state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state = x;
        return x * 2685821657736338717UL;
    }

    internal uint NextU32() {
        return (uint)(NextU64() >> 32);
    }

    internal int NextInt(int exclusiveMax) {
        if (exclusiveMax <= 0) throw new ArgumentOutOfRangeException("exclusiveMax");
        return (int)(NextU64() % (ulong)exclusiveMax);
    }
}

internal sealed class Workload {
    internal string Name;
    internal uint[] Keys;
}

internal sealed class TimingAccumulator {
    internal readonly List<double> RunNs = new List<double>();
    internal readonly List<double> BatchNs = new List<double>();
    internal long Operations;
    internal ulong Checksum;
}

public static class Cq3PrototypeBenchmark {
    private static long SumIndexBytes(string path) {
        long total = 0;
        foreach (var file in Directory.EnumerateFiles(path, "*", SearchOption.AllDirectories)) {
            checked { total += new FileInfo(file).Length; }
        }
        return total;
    }

    private static double Percentile(List<double> values, double p) {
        if (values.Count == 0) return Double.NaN;
        var copy = values.ToArray();
        Array.Sort(copy);
        double rank = p * (copy.Length - 1);
        int lo = (int)Math.Floor(rank);
        int hi = (int)Math.Ceiling(rank);
        if (lo == hi) return copy[lo];
        double fraction = rank - lo;
        return copy[lo] + (copy[hi] - copy[lo]) * fraction;
    }

    private static double Median(List<double> values) {
        return Percentile(values, 0.5);
    }

    private static uint[] MakeHits(SegmentSource source, int count, DeterministicRng rng) {
        uint[] keys = new uint[count];
        for (int i = 0; i < count; i++) keys[i] = source.KeyAtEntry(rng.NextInt(source.Entries));
        return keys;
    }

    private static uint[] MakeMisses(CurrentPrefix10Lookup current, int count, DeterministicRng rng) {
        uint[] keys = new uint[count];
        int written = 0;
        int guard = 0;
        while (written < count) {
            if (++guard > count * 1000) throw new InvalidDataException("unable to generate deterministic CQ3 misses");
            uint key = rng.NextU32() & 0x00ffffffU;
            ProtoMeta meta;
            if (!current.TryLookup(key, out meta)) keys[written++] = key;
        }
        return keys;
    }

    private static void Shuffle(uint[] values, DeterministicRng rng) {
        for (int i = values.Length - 1; i > 0; i--) {
            int j = rng.NextInt(i + 1);
            uint tmp = values[i];
            values[i] = values[j];
            values[j] = tmp;
        }
    }

    private static List<Workload> MakeWorkloads(SegmentSource source, CurrentPrefix10Lookup current, int count, ulong seed) {
        var rng = new DeterministicRng(seed);
        uint[] hits = MakeHits(source, count, rng);
        uint[] misses = MakeMisses(current, count, rng);

        uint[] sortedHits = (uint[])hits.Clone();
        Array.Sort(sortedHits);

        uint[] mixed = new uint[count];
        int hitCount = count / 2;
        for (int i = 0; i < hitCount; i++) mixed[i] = hits[i];
        for (int i = hitCount; i < count; i++) mixed[i] = misses[i - hitCount];
        Shuffle(mixed, rng);

        return new List<Workload> {
            new Workload { Name = "hit-random", Keys = hits },
            new Workload { Name = "miss-random", Keys = misses },
            new Workload { Name = "mixed-random-50", Keys = mixed },
            new Workload { Name = "hit-sorted-locality", Keys = sortedHits }
        };
    }

    private static List<uint> MakeValidationKeys(SegmentSource source, CurrentPrefix10Lookup current, DeterministicRng rng) {
        var set = new HashSet<uint>();

        for (int p = 0; p < 256; p++) {
            int start = (int)source.Prefix[p];
            int end = (int)source.Prefix[p + 1];
            if (start < end) {
                set.Add(source.KeyAtEntry(start));
                set.Add(source.KeyAtEntry(end - 1));
                int middle = start + ((end - start) >> 1);
                set.Add(source.KeyAtEntry(middle));
            }
        }

        int randomHits = Math.Min(20000, source.Entries);
        for (int i = 0; i < randomHits; i++) set.Add(source.KeyAtEntry(rng.NextInt(source.Entries)));

        int misses = 0;
        int guard = 0;
        while (misses < 20000) {
            if (++guard > 20000000) throw new InvalidDataException("validation miss generation guard");
            uint key = rng.NextU32() & 0x00ffffffU;
            ProtoMeta meta;
            if (!current.TryLookup(key, out meta) && set.Add(key)) misses++;
        }
        return set.ToList();
    }

    private static void ValidateEquivalent(
        SegmentSource source,
        CurrentPrefix10Lookup current,
        List<ICq3PrototypeLookup> candidates,
        List<uint> keys) {

        foreach (uint key in keys) {
            ProtoMeta expected;
            bool expectedFound = current.TryLookup(key, out expected);
            foreach (var candidate in candidates) {
                ProtoMeta actual;
                bool actualFound = candidate.TryLookup(key, out actual);
                if (expectedFound != actualFound || !expected.SameAs(actual)) {
                    throw new InvalidDataException(
                        source.File + ": metadata mismatch for key 0x" + key.ToString("x6")
                        + " current=" + expectedFound + "/" + expected.Encoding + "/" + expected.Count + "/" + expected.Offset + "/" + expected.Bytes
                        + " candidate=" + candidate.Name + "=" + actualFound + "/" + actual.Encoding + "/" + actual.Count + "/" + actual.Offset + "/" + actual.Bytes
                    );
                }
            }
        }
    }

    private static void Warmup(ICq3PrototypeLookup lookup, uint[] keys) {
        int n = Math.Min(keys.Length, 4096);
        ulong checksum = 0;
        for (int i = 0; i < n; i++) {
            ProtoMeta meta;
            lookup.TryLookup(keys[i], out meta);
            checksum ^= meta.Digest() + (ulong)i;
        }
        if (checksum == 0x123456789abcdef0UL) Console.WriteLine("warmup checksum sentinel");
    }

    private static void Measure(
        ICq3PrototypeLookup lookup,
        uint[] keys,
        int batchSize,
        TimingAccumulator acc) {

        ulong checksum = 0;
        long elapsedTicks = 0;
        for (int begin = 0; begin < keys.Length; begin += batchSize) {
            int end = Math.Min(keys.Length, begin + batchSize);
            long started = Stopwatch.GetTimestamp();
            for (int i = begin; i < end; i++) {
                ProtoMeta meta;
                lookup.TryLookup(keys[i], out meta);
                checksum ^= meta.Digest() + (ulong)(i + 1);
            }
            long ticks = Stopwatch.GetTimestamp() - started;
            elapsedTicks += ticks;
            double ns = (double)ticks * 1_000_000_000.0 / Stopwatch.Frequency;
            acc.BatchNs.Add(ns / (end - begin));
        }
        double totalNs = (double)elapsedTicks * 1_000_000_000.0 / Stopwatch.Frequency;
        acc.RunNs.Add(totalNs / keys.Length);
        acc.Operations += keys.Length;
        acc.Checksum ^= checksum;
    }

    private static RepresentationSummary Representation(
        string name,
        long bytes,
        long currentDirBytes,
        long indexBytes,
        double encodeMs,
        string model) {

        long whole = checked(indexBytes - currentDirBytes + bytes);
        return new RepresentationSummary {
            Name = name,
            DirectoryBytes = bytes,
            DirectoryMiB = bytes / (1024.0 * 1024.0),
            DirectoryGiB = bytes / (1024.0 * 1024.0 * 1024.0),
            DirectoryReductionPercent = (currentDirBytes - bytes) * 100.0 / currentDirBytes,
            EstimatedWholeIndexBytes = whole,
            EstimatedWholeIndexGiB = whole / (1024.0 * 1024.0 * 1024.0),
            EstimatedWholeIndexReductionPercent = (currentDirBytes - bytes) * 100.0 / indexBytes,
            PrototypeEncodeMs = encodeMs,
            LookupModel = model
        };
    }

    public static PrototypeBenchmarkResult Run(
        string indexPath,
        int queriesPerWorkload,
        int repeats,
        int batchSize,
        long seed) {

        indexPath = Path.GetFullPath(indexPath);
        string[] files = Directory.GetFiles(indexPath, "seg-*.prseg", SearchOption.TopDirectoryOnly)
            .OrderBy(x => x, StringComparer.OrdinalIgnoreCase)
            .ToArray();
        if (files.Length == 0) throw new InvalidDataException("No seg-*.prseg files found.");

        long indexBytes = SumIndexBytes(indexPath);
        var segmentSummaries = new List<SegmentSummary>();
        var accumulators = new Dictionary<string, TimingAccumulator>(StringComparer.Ordinal);
        var encodeTotals = new Dictionary<string, double>(StringComparer.Ordinal) {
            { "current-prefix10", 0.0 },
            { "fixed8-packed14", 0.0 },
            { "blocked-delta-64", 0.0 },
            { "blocked-delta-256", 0.0 }
        };
        var sizeTotals = new Dictionary<string, long>(StringComparer.Ordinal) {
            { "current-prefix10", 0L },
            { "fixed8-packed14", 0L },
            { "blocked-delta-64", 0L },
            { "blocked-delta-256", 0L }
        };

        long totalEntries = 0;
        int totalValidationKeys = 0;

        for (int segmentIndex = 0; segmentIndex < files.Length; segmentIndex++) {
            SegmentSource source = SegmentSource.Open(files[segmentIndex]);
            var current = new CurrentPrefix10Lookup(source);

            long t0 = Stopwatch.GetTimestamp();
            var fixed8 = new Fixed8Lookup(source);
            double fixedMs = (Stopwatch.GetTimestamp() - t0) * 1000.0 / Stopwatch.Frequency;

            t0 = Stopwatch.GetTimestamp();
            var blocked64 = new BlockedDeltaLookup(source, 64);
            double block64Ms = (Stopwatch.GetTimestamp() - t0) * 1000.0 / Stopwatch.Frequency;

            t0 = Stopwatch.GetTimestamp();
            var blocked256 = new BlockedDeltaLookup(source, 256);
            double block256Ms = (Stopwatch.GetTimestamp() - t0) * 1000.0 / Stopwatch.Frequency;

            var reps = new List<ICq3PrototypeLookup> { current, fixed8, blocked64, blocked256 };
            var candidates = new List<ICq3PrototypeLookup> { fixed8, blocked64, blocked256 };

            ulong segmentSeed = unchecked((ulong)seed ^ ((ulong)(segmentIndex + 1) * 0x9e3779b97f4a7c15UL));
            var validation = MakeValidationKeys(source, current, new DeterministicRng(segmentSeed ^ 0xa0761d6478bd642fUL));
            ValidateEquivalent(source, current, candidates, validation);
            totalValidationKeys += validation.Count;

            var workloads = MakeWorkloads(source, current, queriesPerWorkload, segmentSeed);
            foreach (var workload in workloads) {
                foreach (var rep in reps) Warmup(rep, workload.Keys);
                for (int repeat = 0; repeat < repeats; repeat++) {
                    if (repeat > 0) {
                        GC.Collect();
                        GC.WaitForPendingFinalizers();
                        GC.Collect();
                    }
                    int rotate = (repeat + segmentIndex) % reps.Count;
                    for (int order = 0; order < reps.Count; order++) {
                        var rep = reps[(rotate + order) % reps.Count];
                        string key = rep.Name + "|" + workload.Name;
                        TimingAccumulator acc;
                        if (!accumulators.TryGetValue(key, out acc)) {
                            acc = new TimingAccumulator();
                            accumulators.Add(key, acc);
                        }
                        Measure(rep, workload.Keys, batchSize, acc);
                    }
                }
            }

            totalEntries += source.Entries;
            sizeTotals["current-prefix10"] += current.StorageBytes;
            sizeTotals["fixed8-packed14"] += fixed8.StorageBytes;
            sizeTotals["blocked-delta-64"] += blocked64.StorageBytes;
            sizeTotals["blocked-delta-256"] += blocked256.StorageBytes;
            encodeTotals["fixed8-packed14"] += fixedMs;
            encodeTotals["blocked-delta-64"] += block64Ms;
            encodeTotals["blocked-delta-256"] += block256Ms;

            segmentSummaries.Add(new SegmentSummary {
                File = source.File,
                Entries = source.Entries,
                DocCount = source.DocCount,
                UnitCount = source.UnitCount,
                CurrentBytes = current.StorageBytes,
                Fixed8Bytes = fixed8.StorageBytes,
                Blocked64Bytes = blocked64.StorageBytes,
                Blocked256Bytes = blocked256.StorageBytes,
                Fixed8EncodeMs = fixedMs,
                Blocked64EncodeMs = block64Ms,
                Blocked256EncodeMs = block256Ms,
                ValidationKeys = validation.Count
            });

            source = null;
            current = null;
            fixed8 = null;
            blocked64 = null;
            blocked256 = null;
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
        }

        long currentBytes = sizeTotals["current-prefix10"];
        var representations = new List<RepresentationSummary> {
            Representation(
                "current-prefix10", currentBytes, currentBytes, indexBytes, 0.0,
                "Current Prefix10: 257-entry prefix table + fixed 10-byte records + binary search."
            ),
            Representation(
                "fixed8-packed14", sizeTotals["fixed8-packed14"], currentBytes, indexBytes, encodeTotals["fixed8-packed14"],
                "Same prefix partition and binary search with fixed 8-byte records: suffix u16 + encoding/count u16 + offset u32."
            ),
            Representation(
                "blocked-delta-64", sizeTotals["blocked-delta-64"], currentBytes, indexBytes, encodeTotals["blocked-delta-64"],
                "Prefix -> binary search 10-byte block checkpoints -> varint decode within a block of at most 64 records."
            ),
            Representation(
                "blocked-delta-256", sizeTotals["blocked-delta-256"], currentBytes, indexBytes, encodeTotals["blocked-delta-256"],
                "Prefix -> binary search 10-byte block checkpoints -> varint decode within a block of at most 256 records."
            )
        };

        string[] workloadNames = new [] { "hit-random", "miss-random", "mixed-random-50", "hit-sorted-locality" };
        string[] repNames = new [] { "current-prefix10", "fixed8-packed14", "blocked-delta-64", "blocked-delta-256" };
        var timings = new List<AggregateTiming>();

        foreach (string workload in workloadNames) {
            string currentKey = "current-prefix10|" + workload;
            double currentMedian = Median(accumulators[currentKey].RunNs);
            foreach (string repName in repNames) {
                var acc = accumulators[repName + "|" + workload];
                double median = Median(acc.RunNs);
                timings.Add(new AggregateTiming {
                    Representation = repName,
                    Workload = workload,
                    TotalMeasuredOperations = acc.Operations,
                    SegmentRuns = files.Length,
                    RepeatsPerSegment = repeats,
                    MedianRunNsPerOp = median,
                    MinRunNsPerOp = acc.RunNs.Min(),
                    MaxRunNsPerOp = acc.RunNs.Max(),
                    BatchP50NsPerOp = Percentile(acc.BatchNs, 0.50),
                    BatchP95NsPerOp = Percentile(acc.BatchNs, 0.95),
                    BatchP99NsPerOp = Percentile(acc.BatchNs, 0.99),
                    MedianMillionOpsPerSecond = 1000.0 / median,
                    RatioVsCurrentMedian = median / currentMedian,
                    ChecksumXor = acc.Checksum
                });
            }
        }

        return new PrototypeBenchmarkResult {
            SchemaVersion = 1,
            IndexPath = indexPath,
            AnalysisMode = "read-only source PRSEG; candidate CQ3DIR representations are built only in managed memory; no production format is written",
            Format = "PRSEG005",
            DirectoryKind = "Prefix10",
            IndexBytes = indexBytes,
            IndexGiB = indexBytes / (1024.0 * 1024.0 * 1024.0),
            SegmentCount = files.Length,
            Entries = totalEntries,
            CurrentCq3DirBytes = currentBytes,
            CurrentCq3DirGiB = currentBytes / (1024.0 * 1024.0 * 1024.0),
            QueriesPerWorkloadPerSegment = queriesPerWorkload,
            Repeats = repeats,
            BatchSize = batchSize,
            Seed = seed,
            CorrectnessValidationKeys = totalValidationKeys,
            Representations = representations,
            Timings = timings,
            Segments = segmentSummaries,
            Workloads = workloadNames,
            Caveats = new [] {
                "This is a managed C# prototype microbenchmark, not a production Rust/mmap benchmark. Use it to rank designs and reject obviously poor candidates, not as final ACCEPT evidence.",
                "Every candidate returns the same CQ3 metadata (found/encoding/count/offset/bytes) as current Prefix10 for prefix-boundary keys plus deterministic random hits/misses before timing.",
                "Candidate representations exist only in RAM. The PRSEG source is opened read-only and is never rewritten.",
                "Batch percentile values are per-operation time derived from timed batches; they are not single-lookup hardware latency measurements.",
                "Prototype encode time measures conversion from the current CQ3DIR in memory, not integrated production build cost.",
                "GC and JIT effects are reduced by warmup, repeated runs, rotated representation order, and explicit collection between repeats, but OS scheduling and CPU frequency noise remain."
            }
        };
    }
}

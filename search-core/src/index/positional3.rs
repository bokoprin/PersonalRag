use super::positional_frontier::{self, GramKey};
use super::*;

const POS3_MAGIC: &[u8; 8] = b"PRPOS003";
const POS3_HEADER_BYTES: usize = 48;
const POS3_RECORD_BYTES: usize = 40;
const POS3_FOOTER_BYTES: usize = 8;
const POS3_MIN_STORED_GRAM: usize = 9;
const POS3_MAX_GRAM_LIMIT: usize = positional_frontier::MAX_GRAM;
const POS3_UNIT_CACHE_MAX_ENTRIES: usize = 24;

const CODEC_DELTA: u8 = 0;
const CODEC_BITMAP: u8 = 1;
const CODEC_COMPLEMENT: u8 = 2;
const CODEC_ALL: u8 = 3;
const CODEC_RUNS: u8 = 4;
const CODEC_BP128: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pos3Policy {
    Delta,
    Adaptive,
    Bitmap,
    Bp128,
}

impl Pos3Policy {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Delta => "delta",
            Self::Adaptive => "adaptive",
            Self::Bitmap => "bitmap",
            Self::Bp128 => "bp128",
        }
    }

    fn flag(self) -> u32 {
        match self {
            Self::Delta => 0,
            Self::Adaptive => 1,
            Self::Bitmap => 2,
            Self::Bp128 => 3,
        }
    }

    fn from_flag(value: u32) -> Result<Self> {
        match value {
            0 => Ok(Self::Delta),
            1 => Ok(Self::Adaptive),
            2 => Ok(Self::Bitmap),
            3 => Ok(Self::Bp128),
            _ => Err(SearchError::Format("unknown PRPOS003 policy".into())),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pos3BuildReport {
    pub segments: usize,
    pub records: u64,
    pub units: u64,
    pub bytes: u64,
    pub elapsed_ms: f64,
    pub delta_records: u64,
    pub bitmap_records: u64,
    pub complement_records: u64,
    pub all_records: u64,
    pub run_records: u64,
    pub bp128_records: u64,
}

#[derive(Clone, Copy, Debug)]
struct Pos3Record {
    key: GramKey,
    codec: u8,
    unit_count: u32,
    offset: u32,
    bytes: u32,
    checksum: u64,
}

fn append_varint3(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn encode_delta(units: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    let mut prev = 0u32;
    for (i, &unit) in units.iter().enumerate() {
        append_varint3(&mut out, if i == 0 { unit } else { unit - prev });
        prev = unit;
    }
    out
}

fn decode_delta(data: &[u8], expected: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    let mut prev = 0u32;
    for i in 0..expected {
        let mut delta = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = *data
                .get(cursor)
                .ok_or_else(|| SearchError::Format("truncated PRPOS003 varint".into()))?;
            cursor += 1;
            if shift > 28 {
                return Err(SearchError::Format("invalid PRPOS003 varint".into()));
            }
            delta |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let value = if i == 0 {
            delta
        } else {
            prev.checked_add(delta)
                .ok_or_else(|| SearchError::Format("PRPOS003 delta overflow".into()))?
        };
        out.push(value);
        prev = value;
    }
    if cursor != data.len() {
        return Err(SearchError::Format("PRPOS003 varint trailing bytes".into()));
    }
    Ok(out)
}

fn encode_bitmap(units: &[u32], universe: u32) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(universe.div_ceil(8)).unwrap_or(usize::MAX)];
    for &unit in units {
        let index = unit as usize;
        out[index / 8] |= 1u8 << (index % 8);
    }
    out
}

fn decode_bitmap(data: &[u8], expected: usize, universe: u32) -> Result<Vec<u32>> {
    let required = universe.div_ceil(8) as usize;
    if data.len() != required {
        return Err(SearchError::Format("PRPOS003 bitmap size mismatch".into()));
    }
    let mut out = Vec::with_capacity(expected);
    for (word_index, chunk) in data.chunks(8).enumerate() {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        let mut word = u64::from_le_bytes(buf);
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            let unit = word_index * 64 + bit;
            if unit < universe as usize {
                out.push(unit as u32);
            }
            word &= word - 1;
        }
    }
    if out.len() != expected {
        return Err(SearchError::Format("PRPOS003 bitmap count mismatch".into()));
    }
    Ok(out)
}

fn missing_units(units: &[u32], universe: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(universe as usize - units.len());
    let mut i = 0usize;
    for unit in 0..universe {
        if i < units.len() && units[i] == unit {
            i += 1;
        } else {
            out.push(unit);
        }
    }
    out
}

fn encode_runs(units: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    if units.is_empty() {
        return out;
    }
    let mut start = units[0];
    let mut prev = units[0];
    for &unit in &units[1..] {
        if unit == prev + 1 {
            prev = unit;
            continue;
        }
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&(prev - start + 1).to_le_bytes());
        start = unit;
        prev = unit;
    }
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&(prev - start + 1).to_le_bytes());
    out
}

fn decode_runs(data: &[u8], expected: usize) -> Result<Vec<u32>> {
    if !data.len().is_multiple_of(8) {
        return Err(SearchError::Format("PRPOS003 run payload shape".into()));
    }
    let mut out = Vec::with_capacity(expected);
    for off in (0..data.len()).step_by(8) {
        let start = rd32(data, off)?;
        let len = rd32(data, off + 4)?;
        if len == 0 {
            return Err(SearchError::Format("PRPOS003 zero run".into()));
        }
        for unit in start..start.saturating_add(len) {
            out.push(unit);
        }
    }
    if out.len() != expected {
        return Err(SearchError::Format("PRPOS003 run count mismatch".into()));
    }
    Ok(out)
}

fn bit_width(value: u32) -> u8 {
    if value == 0 {
        0
    } else {
        (32 - value.leading_zeros()) as u8
    }
}

fn append_packed_bits(out: &mut Vec<u8>, values: &[u32], width: u8) {
    if width == 0 {
        return;
    }
    let width = u32::from(width);
    let mut acc = 0u64;
    let mut bits = 0u32;
    for &value in values {
        acc |= u64::from(value) << bits;
        bits += width;
        while bits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits != 0 {
        out.push(acc as u8);
    }
}

fn encode_bp128(units: &[u32]) -> Vec<u8> {
    let mut deltas = Vec::with_capacity(units.len());
    let mut prev = 0u32;
    for (i, &unit) in units.iter().enumerate() {
        deltas.push(if i == 0 { unit } else { unit - prev });
        prev = unit;
    }
    let mut out = Vec::new();
    for block in deltas.chunks(128) {
        let max = block.iter().copied().max().unwrap_or(0);
        let width = bit_width(max);
        out.push((block.len() - 1) as u8);
        out.push(width);
        append_packed_bits(&mut out, block, width);
    }
    out
}

fn read_packed_value(data: &[u8], bit: usize, width: u8) -> Result<u32> {
    if width == 0 {
        return Ok(0);
    }
    let byte = bit / 8;
    let shift = bit % 8;
    let needed = (shift + usize::from(width)).div_ceil(8);
    let slice = data
        .get(byte..byte + needed)
        .ok_or_else(|| SearchError::Format("truncated PRPOS003 bp128".into()))?;
    let mut word = 0u64;
    for (i, &b) in slice.iter().enumerate() {
        word |= u64::from(b) << (i * 8);
    }
    let mask = if width == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << width) - 1
    };
    Ok(((word >> shift) & mask) as u32)
}

fn decode_bp128(data: &[u8], expected: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    let mut prev = 0u32;
    while out.len() < expected {
        let count = usize::from(
            *data
                .get(cursor)
                .ok_or_else(|| SearchError::Format("truncated PRPOS003 bp128 header".into()))?,
        ) + 1;
        let width = *data
            .get(cursor + 1)
            .ok_or_else(|| SearchError::Format("truncated PRPOS003 bp128 width".into()))?;
        cursor += 2;
        if width > 32 || out.len() + count > expected {
            return Err(SearchError::Format("invalid PRPOS003 bp128 block".into()));
        }
        let payload_bytes = (count * usize::from(width)).div_ceil(8);
        let payload = data
            .get(cursor..cursor + payload_bytes)
            .ok_or_else(|| SearchError::Format("truncated PRPOS003 bp128 payload".into()))?;
        for i in 0..count {
            let delta = read_packed_value(payload, i * usize::from(width), width)?;
            let value = if out.is_empty() {
                delta
            } else {
                prev.checked_add(delta)
                    .ok_or_else(|| SearchError::Format("PRPOS003 bp128 overflow".into()))?
            };
            out.push(value);
            prev = value;
        }
        cursor += payload_bytes;
    }
    if cursor != data.len() {
        return Err(SearchError::Format("PRPOS003 bp128 trailing bytes".into()));
    }
    Ok(out)
}

fn posting_checksum3(key: &GramKey, codec: u8, units: u32, payload: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in [key.len, codec]
        .into_iter()
        .chain(key.as_slice().iter().copied())
        .chain(units.to_le_bytes())
        .chain(payload.iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn encode_units_policy(units: &[u32], universe: u32, policy: Pos3Policy) -> (u8, Vec<u8>) {
    if units.len() == universe as usize {
        return (CODEC_ALL, Vec::new());
    }
    let delta = encode_delta(units);
    match policy {
        Pos3Policy::Delta => (CODEC_DELTA, delta),
        Pos3Policy::Bitmap => (CODEC_BITMAP, encode_bitmap(units, universe)),
        Pos3Policy::Bp128 => (CODEC_BP128, encode_bp128(units)),
        Pos3Policy::Adaptive => {
            let mut best = (CODEC_DELTA, delta);
            let missing = missing_units(units, universe);
            let complement = encode_delta(&missing);
            // Complement is especially fast for near-saturated postings because decoding the
            // small missing set plus one sequential universe pass is cheaper than decoding a
            // huge positive list. Require both a strong density skew and a byte win.
            if missing.len().saturating_mul(8) <= units.len() && complement.len() + 4 < best.1.len()
            {
                best = (CODEC_COMPLEMENT, complement);
            }
            let runs = encode_runs(units);
            if !runs.is_empty() && runs.len().saturating_mul(2) < best.1.len() {
                best = (CODEC_RUNS, runs);
            }
            let bitmap = encode_bitmap(units, universe);
            // R14 showed dense bitmap iteration loses to delta on ordinary dense postings.
            // Only choose it when the byte reduction is overwhelming.
            if bitmap.len().saturating_mul(4) < best.1.len() {
                best = (CODEC_BITMAP, bitmap);
            }
            let bp128 = encode_bp128(units);
            if bp128.len().saturating_mul(4) < best.1.len().saturating_mul(3) {
                best = (CODEC_BP128, bp128);
            }
            best
        }
    }
}

pub(super) fn build_segment_sidecar3_from_postings<S: SegmentBuildSource>(
    segment: &S,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    all_keys: &[GramKey],
    postings: &[Vec<u32>],
) -> Result<Vec<u8>> {
    if q3_threshold_ppm > 1_000_000
        || child_threshold_ppm > 1_000_000
        || !(4..=POS3_MAX_GRAM_LIMIT).contains(&max_gram)
    {
        return Err(SearchError::Format("invalid PRPOS003 configuration".into()));
    }
    let minimum_child = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(child_threshold_ppm))
        .div_ceil(1_000_000) as usize;

    let mut records = Vec::with_capacity(all_keys.len());
    let mut payload = Vec::new();
    for (&key, units) in all_keys.iter().zip(postings) {
        // q4..q8 are frontier-only: PRPOS002 owns that exact-query range. A combined build may
        // retain a lower-threshold q4..q8 frontier for PRPOS002, so re-check PRPOS003 density
        // defensively before serializing q9..q16.
        if usize::from(key.len) < POS3_MIN_STORED_GRAM
            || usize::from(key.len) > max_gram
            || units.len() < minimum_child
        {
            continue;
        }
        let (codec, encoded) = encode_units_policy(units, segment.build_unit_count(), policy);
        let offset = u32::try_from(payload.len())
            .map_err(|_| SearchError::Format("PRPOS003 payload >4GiB".into()))?;
        let bytes = u32::try_from(encoded.len())
            .map_err(|_| SearchError::Format("PRPOS003 record >4GiB".into()))?;
        let unit_count = u32::try_from(units.len())
            .map_err(|_| SearchError::Format("PRPOS003 unit count overflow".into()))?;
        records.push(Pos3Record {
            key,
            codec,
            unit_count,
            offset,
            bytes,
            checksum: posting_checksum3(&key, codec, unit_count, &encoded),
        });
        payload.extend_from_slice(&encoded);
    }

    let mut out = Vec::with_capacity(
        POS3_HEADER_BYTES + records.len() * POS3_RECORD_BYTES + payload.len() + POS3_FOOTER_BYTES,
    );
    out.extend_from_slice(POS3_MAGIC);
    out.extend_from_slice(&segment.build_unit_count().to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    out.extend_from_slice(&q3_threshold_ppm.to_le_bytes());
    out.extend_from_slice(&child_threshold_ppm.to_le_bytes());
    out.extend_from_slice(&(max_gram as u32).to_le_bytes());
    out.extend_from_slice(&policy.flag().to_le_bytes());
    out.extend_from_slice(&segment.build_segment_checksum()?.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    for rec in &records {
        out.push(rec.key.len);
        out.push(rec.codec);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&rec.key.bytes);
        out.extend_from_slice(&rec.unit_count.to_le_bytes());
        out.extend_from_slice(&rec.offset.to_le_bytes());
        out.extend_from_slice(&rec.bytes.to_le_bytes());
        out.extend_from_slice(&rec.checksum.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    let checksum = fnv1a(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

fn build_segment_sidecar3(
    segment: &SegmentReader,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
) -> Result<Vec<u8>> {
    if q3_threshold_ppm > 1_000_000
        || child_threshold_ppm > 1_000_000
        || !(4..=POS3_MAX_GRAM_LIMIT).contains(&max_gram)
    {
        return Err(SearchError::Format("invalid PRPOS003 configuration".into()));
    }
    let dense_q3 = positional_frontier::dense_q3_keys(segment, q3_threshold_ppm)?;
    let minimum_child = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(child_threshold_ppm))
        .div_ceil(1_000_000) as u32;
    let (all_keys, postings) = positional_frontier::build_shared_postings(
        segment,
        &dense_q3,
        minimum_child,
        minimum_child,
        max_gram,
        false,
    )?;
    build_segment_sidecar3_from_postings(
        segment,
        q3_threshold_ppm,
        child_threshold_ppm,
        max_gram,
        policy,
        &all_keys,
        &postings,
    )
}

pub(super) fn sidecar3_path(segment_path: &Path) -> PathBuf {
    segment_path.with_extension("pos3")
}

pub(super) fn inspect_sidecar3(
    path: &Path,
    segment: &SegmentReader,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
) -> Result<(u64, u64, u64, [u64; 6])> {
    let side = Pos3Sidecar::open_verified(path, segment)?;
    if side.q3_threshold_ppm != q3_threshold_ppm
        || side.child_threshold_ppm != child_threshold_ppm
        || side.max_gram != max_gram
        || side.policy != policy
    {
        return Err(SearchError::Format(
            "existing PRPOS003 configuration mismatch".into(),
        ));
    }
    Ok((
        fs::metadata(path)?.len(),
        u64::from(side.records),
        side.total_units()?,
        side.codec_counts()?,
    ))
}

#[derive(Clone, Copy)]
struct Pos3BuildConfig {
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    durable: bool,
}

fn build_one_sidecar3(
    root: &Path,
    entry: &ManifestSegment,
    index: usize,
    config: Pos3BuildConfig,
) -> Result<(u64, u64, u64, [u64; 6])> {
    let Pos3BuildConfig {
        q3_threshold_ppm,
        child_threshold_ppm,
        max_gram,
        policy,
        durable,
    } = config;
    let segment_path = root.join(&entry.file);
    let segment = SegmentReader::open(&segment_path, false)?;
    let path = sidecar3_path(&segment_path);
    if path.exists() {
        return inspect_sidecar3(
            &path,
            &segment,
            q3_threshold_ppm,
            child_threshold_ppm,
            max_gram,
            policy,
        );
    }
    let bytes = build_segment_sidecar3(
        &segment,
        q3_threshold_ppm,
        child_threshold_ppm,
        max_gram,
        policy,
    )?;
    let tmp = path.with_extension(format!("pos3.tmp-{}-{index}", std::process::id()));
    let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    let publish = (|| -> Result<()> {
        f.write_all(&bytes)?;
        if durable {
            f.sync_all()?;
        }
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        }
        fs::rename(&tmp, &path)?;
        if durable {
            #[cfg(unix)]
            fs::File::open(root)?.sync_all()?;
        }
        Ok(())
    })();
    if publish.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    publish?;
    inspect_sidecar3(
        &path,
        &segment,
        q3_threshold_ppm,
        child_threshold_ppm,
        max_gram,
        policy,
    )
}

pub fn build_positional3_sidecars(
    root: impl AsRef<Path>,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    durable: bool,
) -> Result<Pos3BuildReport> {
    let started = Instant::now();
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    if manifest.is_empty() {
        return Ok(Pos3BuildReport::default());
    }
    // Frontier memory is bounded per 5k-doc segment; four segment builders stayed well below
    // the 4 GiB sandbox limit and materially improve 500k+ build throughput.
    let workers = manifest.len().clamp(1, 4);
    let next = AtomicUsize::new(0);
    let config = Pos3BuildConfig {
        q3_threshold_ppm,
        child_threshold_ppm,
        max_gram,
        policy,
        durable,
    };
    type R = Option<Result<(u64, u64, u64, [u64; 6])>>;
    let results: Arc<Mutex<Vec<R>>> = Arc::new(Mutex::new(
        std::iter::repeat_with(|| None)
            .take(manifest.len())
            .collect(),
    ));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let results = Arc::clone(&results);
            let next = &next;
            let manifest = &manifest;
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if i >= manifest.len() {
                        break;
                    }
                    let r = build_one_sidecar3(root, &manifest[i], i, config);
                    results.lock().expect("PRPOS003 build result poisoned")[i] = Some(r);
                }
            });
        }
    });
    let mut report = Pos3BuildReport::default();
    for r in Arc::try_unwrap(results)
        .map_err(|_| SearchError::Format("PRPOS003 result ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("PRPOS003 result mutex poisoned".into()))?
    {
        let (bytes, records, units, codecs) =
            r.ok_or_else(|| SearchError::Format("PRPOS003 segment not built".into()))??;
        report.segments += 1;
        report.bytes += bytes;
        report.records += records;
        report.units += units;
        report.delta_records += codecs[0];
        report.bitmap_records += codecs[1];
        report.complement_records += codecs[2];
        report.all_records += codecs[3];
        report.run_records += codecs[4];
        report.bp128_records += codecs[5];
    }
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

struct Pos3UnitCache {
    values: std::collections::HashMap<GramKey, Arc<Vec<u32>>>,
    order: std::collections::VecDeque<GramKey>,
}

pub(super) struct Pos3Sidecar {
    mapped: MappedFile,
    records: u32,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    unit_count: u32,
    payload_offset: usize,
    cache: Mutex<Pos3UnitCache>,
}

impl Pos3Sidecar {
    pub(super) fn open_fast(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open(path, segment, false)
    }

    fn open_verified(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open(path, segment, true)
    }

    fn open(path: &Path, segment: &SegmentReader, verify_all: bool) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        let data = mapped.as_slice();
        if data.len() < POS3_HEADER_BYTES + POS3_FOOTER_BYTES || &data[..8] != POS3_MAGIC {
            return Err(SearchError::Format("bad PRPOS003 header".into()));
        }
        let unit_count = rd32(data, 8)?;
        if unit_count != segment.unit_count() {
            return Err(SearchError::Format("PRPOS003 unit mismatch".into()));
        }
        let records = rd32(data, 12)?;
        let q3_threshold_ppm = rd32(data, 16)?;
        let child_threshold_ppm = rd32(data, 20)?;
        let max_gram = rd32(data, 24)? as usize;
        if !(4..=POS3_MAX_GRAM_LIMIT).contains(&max_gram) {
            return Err(SearchError::Format("PRPOS003 max gram mismatch".into()));
        }
        let policy = Pos3Policy::from_flag(rd32(data, 28)?)?;
        if rd64(data, 32)? != segment.stored_checksum()? {
            return Err(SearchError::Format(
                "PRPOS003 segment checksum mismatch".into(),
            ));
        }
        let payload_bytes = usize::try_from(rd64(data, 40)?)
            .map_err(|_| SearchError::Format("PRPOS003 payload too large".into()))?;
        let payload_offset = POS3_HEADER_BYTES
            .checked_add(records as usize * POS3_RECORD_BYTES)
            .ok_or_else(|| SearchError::Format("PRPOS003 layout overflow".into()))?;
        let footer = payload_offset
            .checked_add(payload_bytes)
            .ok_or_else(|| SearchError::Format("PRPOS003 payload overflow".into()))?;
        if footer + POS3_FOOTER_BYTES != data.len() {
            return Err(SearchError::Format("PRPOS003 length mismatch".into()));
        }
        if fnv1a(&data[..footer]) != rd64(data, footer)? {
            return Err(SearchError::Format("PRPOS003 checksum mismatch".into()));
        }
        let side = Self {
            mapped,
            records,
            q3_threshold_ppm,
            child_threshold_ppm,
            max_gram,
            policy,
            unit_count,
            payload_offset,
            cache: Mutex::new(Pos3UnitCache {
                values: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
        };
        let mut previous: Option<GramKey> = None;
        for i in 0..records as usize {
            let rec = side.record_at(i)?;
            if !(POS3_MIN_STORED_GRAM..=max_gram).contains(&usize::from(rec.key.len)) {
                return Err(SearchError::Format("PRPOS003 bad record gram".into()));
            }
            if let Some(prev) = previous
                && prev >= rec.key
            {
                return Err(SearchError::Format("PRPOS003 records unsorted".into()));
            }
            previous = Some(rec.key);
            let end = rec.offset as usize + rec.bytes as usize;
            if end > payload_bytes {
                return Err(SearchError::Format("PRPOS003 record range invalid".into()));
            }
            if verify_all {
                side.checked_payload(rec)?;
                let units = side.decode_units_uncached(rec)?;
                if units.windows(2).any(|pair| pair[0] >= pair[1])
                    || units.last().is_some_and(|&v| v >= unit_count)
                {
                    return Err(SearchError::Format("PRPOS003 units invalid".into()));
                }
            }
        }
        Ok(side)
    }

    fn data(&self) -> &[u8] {
        self.mapped.as_slice()
    }

    fn record_at(&self, index: usize) -> Result<Pos3Record> {
        if index >= self.records as usize {
            return Err(SearchError::Format("PRPOS003 record OOB".into()));
        }
        let off = POS3_HEADER_BYTES + index * POS3_RECORD_BYTES;
        let len = *self
            .data()
            .get(off)
            .ok_or_else(|| SearchError::Format("PRPOS003 record truncated".into()))?;
        let codec = *self
            .data()
            .get(off + 1)
            .ok_or_else(|| SearchError::Format("PRPOS003 record truncated".into()))?;
        let mut bytes = [0u8; POS3_MAX_GRAM_LIMIT];
        bytes.copy_from_slice(
            self.data()
                .get(off + 4..off + 20)
                .ok_or_else(|| SearchError::Format("PRPOS003 key truncated".into()))?,
        );
        Ok(Pos3Record {
            key: GramKey { len, bytes },
            codec,
            unit_count: rd32(self.data(), off + 20)?,
            offset: rd32(self.data(), off + 24)?,
            bytes: rd32(self.data(), off + 28)?,
            checksum: rd64(self.data(), off + 32)?,
        })
    }

    fn find_record(&self, query: &[u8]) -> Result<Option<Pos3Record>> {
        if !(POS3_MIN_STORED_GRAM..=self.max_gram).contains(&query.len()) {
            return Ok(None);
        }
        let target = GramKey::new(query);
        let mut lo = 0usize;
        let mut hi = self.records as usize;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let rec = self.record_at(mid)?;
            match rec.key.cmp(&target) {
                CmpOrdering::Less => lo = mid + 1,
                CmpOrdering::Greater => hi = mid,
                CmpOrdering::Equal => return Ok(Some(rec)),
            }
        }
        Ok(None)
    }

    pub(super) fn has_exact_record(&self, query: &[u8]) -> Result<bool> {
        let query = fold_ascii(query);
        Ok(self.find_record(&query)?.is_some())
    }

    fn payload(&self, rec: Pos3Record) -> Result<&[u8]> {
        let begin = self.payload_offset + rec.offset as usize;
        let end = begin + rec.bytes as usize;
        self.data()
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("PRPOS003 payload OOB".into()))
    }

    fn checked_payload(&self, rec: Pos3Record) -> Result<&[u8]> {
        let payload = self.payload(rec)?;
        if posting_checksum3(&rec.key, rec.codec, rec.unit_count, payload) != rec.checksum {
            return Err(SearchError::Format(
                "PRPOS003 posting checksum mismatch".into(),
            ));
        }
        Ok(payload)
    }

    fn decode_units_uncached(&self, rec: Pos3Record) -> Result<Vec<u32>> {
        let payload = self.checked_payload(rec)?;
        match rec.codec {
            CODEC_DELTA => decode_delta(payload, rec.unit_count as usize),
            CODEC_BITMAP => decode_bitmap(payload, rec.unit_count as usize, self.unit_count),
            CODEC_COMPLEMENT => {
                let missing_count = self.unit_count.saturating_sub(rec.unit_count) as usize;
                let missing = decode_delta(payload, missing_count)?;
                let mut out = Vec::with_capacity(rec.unit_count as usize);
                let mut cursor = 0usize;
                for unit in 0..self.unit_count {
                    if cursor < missing.len() && missing[cursor] == unit {
                        cursor += 1;
                    } else {
                        out.push(unit);
                    }
                }
                Ok(out)
            }
            CODEC_ALL => {
                if !payload.is_empty() || rec.unit_count != self.unit_count {
                    return Err(SearchError::Format("invalid PRPOS003 all posting".into()));
                }
                Ok((0..self.unit_count).collect())
            }
            CODEC_RUNS => decode_runs(payload, rec.unit_count as usize),
            CODEC_BP128 => decode_bp128(payload, rec.unit_count as usize),
            _ => Err(SearchError::Format("unknown PRPOS003 codec".into())),
        }
    }

    fn decode_units(&self, rec: Pos3Record) -> Result<Arc<Vec<u32>>> {
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| SearchError::Format("PRPOS003 cache poisoned".into()))?;
            if let Some(value) = cache.values.get(&rec.key).cloned() {
                if let Some(i) = cache.order.iter().position(|key| *key == rec.key) {
                    cache.order.remove(i);
                }
                cache.order.push_back(rec.key);
                return Ok(value);
            }
        }
        let units = Arc::new(self.decode_units_uncached(rec)?);
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| SearchError::Format("PRPOS003 cache poisoned".into()))?;
        while cache.values.len() >= POS3_UNIT_CACHE_MAX_ENTRIES {
            if let Some(key) = cache.order.pop_front() {
                cache.values.remove(&key);
            } else {
                break;
            }
        }
        cache.values.insert(rec.key, Arc::clone(&units));
        cache.order.push_back(rec.key);
        Ok(units)
    }

    fn total_units(&self) -> Result<u64> {
        let mut total = 0u64;
        for i in 0..self.records as usize {
            total += u64::from(self.record_at(i)?.unit_count);
        }
        Ok(total)
    }

    fn codec_counts(&self) -> Result<[u64; 6]> {
        let mut out = [0u64; 6];
        for i in 0..self.records as usize {
            let codec = usize::from(self.record_at(i)?.codec);
            if codec >= out.len() {
                return Err(SearchError::Format("unknown PRPOS003 codec".into()));
            }
            out[codec] += 1;
        }
        Ok(out)
    }
}

fn units_to_docs3(segment: &SegmentReader, units: &[u32]) -> Result<Vec<u32>> {
    let mut docs = Vec::new();
    for &unit in units {
        let begin = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
        let end = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
        for i in begin..end {
            docs.push(segment.doc_base() + segment.u32_at(section::UNIT_DOCS, i)?);
        }
    }
    docs.sort_unstable();
    docs.dedup();
    Ok(docs)
}

pub(super) fn search_segment_positional3(
    segment: &SegmentReader,
    side: &Pos3Sidecar,
    query: &[u8],
) -> Result<Option<Vec<u32>>> {
    let query = fold_ascii(query);
    let Some(rec) = side.find_record(&query)? else {
        return Ok(None);
    };
    let units = side.decode_units(rec)?;
    units_to_docs3(segment, units.as_slice()).map(Some)
}

pub(super) fn first_n_segment_positional3(
    segment: &SegmentReader,
    side: &Pos3Sidecar,
    query: &[u8],
    limit: usize,
) -> Result<Option<Vec<u32>>> {
    if limit == 0 {
        return Ok(Some(Vec::new()));
    }
    let query = fold_ascii(query);
    let Some(rec) = side.find_record(&query)? else {
        return Ok(None);
    };
    let units = side.decode_units(rec)?;
    let mut matched = vec![false; segment.unit_count() as usize];
    for &unit in units.iter() {
        matched[unit as usize] = true;
    }
    let mut out = Vec::with_capacity(limit.min(segment.doc_count as usize));
    for doc in 0..segment.doc_count {
        let unit = segment.u32_at(section::DOC_UNIT, u64::from(doc))?;
        if matched[unit as usize] {
            out.push(segment.doc_base() + doc);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(Some(out))
}

pub struct Positional3Index {
    segments: Vec<SegmentReader>,
    sidecars: Vec<Pos3Sidecar>,
}

impl Positional3Index {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
        validate_manifest_ranges(docs, &manifest)?;
        let mut segments = Vec::with_capacity(manifest.len());
        let mut sidecars = Vec::with_capacity(manifest.len());
        for entry in manifest {
            let path = root.join(entry.file);
            let segment = SegmentReader::open(&path, false)?;
            let sidecar = Pos3Sidecar::open_fast(&sidecar3_path(&path), &segment)?;
            segments.push(segment);
            sidecars.push(sidecar);
        }
        Ok(Self { segments, sidecars })
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>, workers: usize) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        let workers = workers.max(1).min(self.segments.len().max(1));
        let next = AtomicUsize::new(0);
        type R = Option<Result<Vec<u32>>>;
        let results: Arc<Mutex<Vec<R>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.segments.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let results = Arc::clone(&results);
                let next = &next;
                let query = &query;
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if i >= self.segments.len() {
                            break;
                        }
                        let r = match search_segment_positional3(
                            &self.segments[i],
                            &self.sidecars[i],
                            query,
                        ) {
                            Ok(Some(hits)) => Ok(hits),
                            Ok(None) => self.segments[i].search_content(query),
                            Err(error) => Err(error),
                        };
                        results.lock().expect("PRPOS003 query result poisoned")[i] = Some(r);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("PRPOS003 result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("PRPOS003 result poisoned".into()))?;
        super::merge_ordered_segment_results(results, "PRPOS003 segment not queried")
    }

    pub fn first_n(&self, query: impl AsRef<[u8]>, limit: usize) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        let mut out = Vec::with_capacity(limit);
        for (segment, sidecar) in self.segments.iter().zip(&self.sidecars) {
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            match first_n_segment_positional3(segment, sidecar, &query, remaining)? {
                Some(hits) => out.extend(hits),
                None => out.extend(segment.first_n(&query, false, remaining)?),
            }
        }
        Ok(out)
    }
}

pub fn verify_positional3_sidecars(root: impl AsRef<Path>) -> Result<()> {
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    for entry in manifest {
        let segment_path = root.join(entry.file);
        let segment = SegmentReader::open(&segment_path, false)?;
        let sidecar_path = sidecar3_path(&segment_path);
        if !sidecar_path.exists() {
            return Err(SearchError::Format(format!(
                "missing PRPOS003 sidecar {}",
                sidecar_path.display()
            )));
        }
        Pos3Sidecar::open_verified(&sidecar_path, &segment)?;
    }
    Ok(())
}

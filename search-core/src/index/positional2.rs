use super::positional_frontier::{self, GramKey};
use super::*;

const POS2_MAGIC: &[u8; 8] = b"PRPOS002";
const POS2_HEADER_BYTES: usize = 48;
const POS2_RECORD_BYTES: usize = 32;
const POS2_FOOTER_BYTES: usize = 8;
const POS2_MAX_GRAM: usize = 8;
const POS2_UNIT_CACHE_MAX_ENTRIES: usize = 24;

#[derive(Clone, Copy, Debug, Default)]
pub struct Pos2BuildReport {
    pub segments: usize,
    pub records: u64,
    pub units: u64,
    pub occurrences: u64,
    pub bytes: u64,
    pub elapsed_ms: f64,
}

#[derive(Clone, Copy, Debug)]
struct Pos2Record {
    key: u64,
    unit_count: u32,
    occurrence_count: u32,
    offset: u32,
    bytes: u32,
    checksum: u64,
}

fn gram_key(bytes: &[u8]) -> u64 {
    debug_assert!((3..=POS2_MAX_GRAM).contains(&bytes.len()));
    let mut key = (bytes.len() as u64) << 56;
    for (i, &byte) in bytes.iter().enumerate() {
        key |= u64::from(byte) << (i * 8);
    }
    key
}

fn append_varint2(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn decode_varints_exact(data: &[u8], expected: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    let mut prev = 0u32;
    for i in 0..expected {
        let mut delta = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = *data
                .get(cursor)
                .ok_or_else(|| SearchError::Format("truncated PRPOS002 varint".into()))?;
            cursor += 1;
            if shift > 28 {
                return Err(SearchError::Format("invalid PRPOS002 varint".into()));
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
                .ok_or_else(|| SearchError::Format("PRPOS002 delta overflow".into()))?
        };
        out.push(value);
        prev = value;
    }
    if cursor != data.len() {
        return Err(SearchError::Format("PRPOS002 varint trailing bytes".into()));
    }
    Ok(out)
}

fn posting_checksum2(key: u64, units: u32, occurrences: u32, payload: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in key
        .to_le_bytes()
        .into_iter()
        .chain(units.to_le_bytes())
        .chain(occurrences.to_le_bytes())
        .chain(payload.iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn encode_pos2_units(unit_ids: &[u32]) -> Result<(Vec<u8>, u32, u32)> {
    let mut unit_stream = Vec::new();
    let mut prev = 0u32;
    for (i, &unit) in unit_ids.iter().enumerate() {
        append_varint2(&mut unit_stream, if i == 0 { unit } else { unit - prev });
        prev = unit;
    }
    let unit_count = u32::try_from(unit_ids.len())
        .map_err(|_| SearchError::Format("PRPOS002 unit count overflow".into()))?;
    let mut payload = Vec::with_capacity(8 + unit_stream.len());
    payload.extend_from_slice(&(unit_stream.len() as u32).to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&unit_stream);
    // q4..q8 records are unit-only, so occurrence_count intentionally equals unit_count.
    Ok((payload, unit_count, unit_count))
}

fn merge_sorted_units(lists: &[&[u32]]) -> Vec<u32> {
    let capacity = lists.iter().map(|list| list.len()).sum();
    let mut out = Vec::with_capacity(capacity);
    for list in lists {
        out.extend_from_slice(list);
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub(super) fn build_segment_sidecar2_from_postings<S: SegmentBuildSource>(
    segment: &S,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    keys: &[GramKey],
    postings: &[Vec<u32>],
) -> Result<Vec<u8>> {
    if q3_threshold_ppm > 1_000_000 || child_threshold_ppm > 1_000_000 {
        return Err(SearchError::Format("PRPOS002 threshold >100%".into()));
    }
    let minimum_child = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(child_threshold_ppm))
        .div_ceil(1_000_000) as usize;

    // PRPOS002's frozen packed-u64 q8 key aliases bit 3 of the final byte with the length tag.
    // Preserve that historical safe-false-positive behavior byte-for-byte by grouping exact
    // shared-frontier keys into their legacy key and unioning units before the density cutoff.
    let mut grouped = std::collections::BTreeMap::<u64, Vec<&[u32]>>::new();
    for (key, units) in keys.iter().zip(postings) {
        if (4..=POS2_MAX_GRAM).contains(&usize::from(key.len)) {
            grouped
                .entry(gram_key(key.as_slice()))
                .or_default()
                .push(units);
        }
    }

    let mut records = Vec::with_capacity(grouped.len());
    let mut payload = Vec::new();
    for (key, lists) in grouped {
        let merged;
        let units = if lists.len() == 1 {
            lists[0]
        } else {
            merged = merge_sorted_units(&lists);
            &merged
        };
        if units.len() < minimum_child {
            continue;
        }
        let (encoded, unit_count, occurrence_count) = encode_pos2_units(units)?;
        let offset = u32::try_from(payload.len())
            .map_err(|_| SearchError::Format("PRPOS002 payload >4GiB".into()))?;
        let bytes = u32::try_from(encoded.len())
            .map_err(|_| SearchError::Format("PRPOS002 record >4GiB".into()))?;
        records.push(Pos2Record {
            key,
            unit_count,
            occurrence_count,
            offset,
            bytes,
            checksum: posting_checksum2(key, unit_count, occurrence_count, &encoded),
        });
        payload.extend_from_slice(&encoded);
    }

    let mut out = Vec::with_capacity(
        POS2_HEADER_BYTES + records.len() * POS2_RECORD_BYTES + payload.len() + POS2_FOOTER_BYTES,
    );
    out.extend_from_slice(POS2_MAGIC);
    out.extend_from_slice(&segment.build_unit_count().to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    out.extend_from_slice(&q3_threshold_ppm.to_le_bytes());
    out.extend_from_slice(&child_threshold_ppm.to_le_bytes());
    out.extend_from_slice(&(POS2_MAX_GRAM as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&segment.build_segment_checksum()?.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    for rec in &records {
        out.extend_from_slice(&rec.key.to_le_bytes());
        out.extend_from_slice(&rec.unit_count.to_le_bytes());
        out.extend_from_slice(&rec.occurrence_count.to_le_bytes());
        out.extend_from_slice(&rec.offset.to_le_bytes());
        out.extend_from_slice(&rec.bytes.to_le_bytes());
        out.extend_from_slice(&rec.checksum.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    let checksum = fnv1a(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

fn build_segment_sidecar2(
    segment: &SegmentReader,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
) -> Result<Vec<u8>> {
    if q3_threshold_ppm > 1_000_000 || child_threshold_ppm > 1_000_000 {
        return Err(SearchError::Format("PRPOS002 threshold >100%".into()));
    }
    let dense_q3 = positional_frontier::dense_q3_keys(segment, q3_threshold_ppm)?;
    let minimum_child = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(child_threshold_ppm))
        .div_ceil(1_000_000) as u32;
    let (keys, postings) = positional_frontier::build_shared_postings(
        segment,
        &dense_q3,
        minimum_child,
        minimum_child,
        POS2_MAX_GRAM,
        true,
    )?;
    build_segment_sidecar2_from_postings(
        segment,
        q3_threshold_ppm,
        child_threshold_ppm,
        &keys,
        &postings,
    )
}

pub(super) fn sidecar2_path(segment_path: &Path) -> PathBuf {
    segment_path.with_extension("pos2")
}

pub(super) fn inspect_sidecar2(
    path: &Path,
    segment: &SegmentReader,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
) -> Result<(u64, u64, u64, u64)> {
    let side = Pos2Sidecar::open_verified(path, segment)?;
    if side.q3_threshold_ppm != q3_threshold_ppm || side.child_threshold_ppm != child_threshold_ppm
    {
        return Err(SearchError::Format(
            "existing PRPOS002 configuration mismatch".into(),
        ));
    }
    Ok((
        fs::metadata(path)?.len(),
        u64::from(side.records),
        side.total_units()?,
        side.total_occurrences()?,
    ))
}

fn build_one_sidecar2(
    root: &Path,
    entry: &ManifestSegment,
    index: usize,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    durable: bool,
) -> Result<(u64, u64, u64, u64)> {
    let segment_path = root.join(&entry.file);
    let segment = SegmentReader::open(&segment_path, false)?;
    let path = sidecar2_path(&segment_path);
    if path.exists() {
        return inspect_sidecar2(&path, &segment, q3_threshold_ppm, child_threshold_ppm);
    }
    let bytes = build_segment_sidecar2(&segment, q3_threshold_ppm, child_threshold_ppm)?;
    let tmp = path.with_extension(format!("pos2.tmp-{}-{index}", std::process::id()));
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
    inspect_sidecar2(&path, &segment, q3_threshold_ppm, child_threshold_ppm)
}

pub fn build_positional2_sidecars(
    root: impl AsRef<Path>,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    durable: bool,
) -> Result<Pos2BuildReport> {
    let started = Instant::now();
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    if manifest.is_empty() {
        return Ok(Pos2BuildReport::default());
    }
    // Two workers cap peak build memory: variable-gram presence maps are much larger than PRPOS001.
    let workers = manifest.len().clamp(1, 4);
    let next = AtomicUsize::new(0);
    type R = Option<Result<(u64, u64, u64, u64)>>;
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
                    let r = build_one_sidecar2(
                        root,
                        &manifest[i],
                        i,
                        q3_threshold_ppm,
                        child_threshold_ppm,
                        durable,
                    );
                    results
                        .lock()
                        .expect("PRPOS002 build result mutex poisoned")[i] = Some(r);
                }
            });
        }
    });
    let mut report = Pos2BuildReport::default();
    for r in Arc::try_unwrap(results)
        .map_err(|_| SearchError::Format("PRPOS002 result ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("PRPOS002 result mutex poisoned".into()))?
    {
        let (bytes, records, units, occ) =
            r.ok_or_else(|| SearchError::Format("PRPOS002 segment not built".into()))??;
        report.segments += 1;
        report.bytes += bytes;
        report.records += records;
        report.units += units;
        report.occurrences += occ;
    }
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

struct Pos2UnitCache {
    values: std::collections::HashMap<u64, Arc<Vec<u32>>>,
    order: std::collections::VecDeque<u64>,
}

pub(super) struct Pos2Sidecar {
    mapped: MappedFile,
    records: u32,
    q3_threshold_ppm: u32,
    child_threshold_ppm: u32,
    payload_offset: usize,
    cache: Mutex<Pos2UnitCache>,
}

impl Pos2Sidecar {
    pub(super) fn open_fast(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open_impl(path, segment, false)
    }
    fn open_verified(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open_impl(path, segment, true)
    }
    fn open_impl(path: &Path, segment: &SegmentReader, verify: bool) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        let data = mapped.as_slice();
        if data.len() < POS2_HEADER_BYTES + POS2_FOOTER_BYTES
            || data.get(..8) != Some(POS2_MAGIC.as_slice())
        {
            return Err(SearchError::Format("bad PRPOS002 header".into()));
        }
        if rd32(data, 8)? != segment.unit_count() {
            return Err(SearchError::Format("PRPOS002 unit mismatch".into()));
        }
        let records = rd32(data, 12)?;
        let q3_threshold_ppm = rd32(data, 16)?;
        let child_threshold_ppm = rd32(data, 20)?;
        if rd32(data, 24)? as usize != POS2_MAX_GRAM {
            return Err(SearchError::Format("PRPOS002 max gram mismatch".into()));
        }
        if rd64(data, 32)? != segment.stored_checksum()? {
            return Err(SearchError::Format(
                "PRPOS002 segment checksum mismatch".into(),
            ));
        }
        let payload_bytes = usize::try_from(rd64(data, 40)?)
            .map_err(|_| SearchError::Format("PRPOS002 payload too large".into()))?;
        let payload_offset = POS2_HEADER_BYTES
            .checked_add(records as usize * POS2_RECORD_BYTES)
            .ok_or_else(|| SearchError::Format("PRPOS002 layout overflow".into()))?;
        let payload_end = payload_offset
            .checked_add(payload_bytes)
            .ok_or_else(|| SearchError::Format("PRPOS002 payload overflow".into()))?;
        if payload_end + POS2_FOOTER_BYTES != data.len() {
            return Err(SearchError::Format("PRPOS002 length mismatch".into()));
        }
        if verify && fnv1a(&data[..payload_end]) != rd64(data, payload_end)? {
            return Err(SearchError::Format("PRPOS002 checksum mismatch".into()));
        }
        let side = Self {
            mapped,
            records,
            q3_threshold_ppm,
            child_threshold_ppm,
            payload_offset,
            cache: Mutex::new(Pos2UnitCache {
                values: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
        };
        side.validate_records()?;
        Ok(side)
    }
    fn data(&self) -> &[u8] {
        self.mapped.as_slice()
    }
    fn record_at(&self, index: usize) -> Result<Pos2Record> {
        if index >= self.records as usize {
            return Err(SearchError::Format("PRPOS002 record OOB".into()));
        }
        let off = POS2_HEADER_BYTES + index * POS2_RECORD_BYTES;
        Ok(Pos2Record {
            key: rd64(self.data(), off)?,
            unit_count: rd32(self.data(), off + 8)?,
            occurrence_count: rd32(self.data(), off + 12)?,
            offset: rd32(self.data(), off + 16)?,
            bytes: rd32(self.data(), off + 20)?,
            checksum: rd64(self.data(), off + 24)?,
        })
    }
    fn validate_records(&self) -> Result<()> {
        let payload_len = self.data().len() - self.payload_offset - POS2_FOOTER_BYTES;
        let mut prev = None;
        let mut end = 0u32;
        for i in 0..self.records as usize {
            let r = self.record_at(i)?;
            if prev.is_some_and(|p| r.key <= p) {
                return Err(SearchError::Format("PRPOS002 records unsorted".into()));
            }
            if r.offset < end || (r.offset as u64 + r.bytes as u64) > payload_len as u64 {
                return Err(SearchError::Format("PRPOS002 record range invalid".into()));
            }
            prev = Some(r.key);
            end = r.offset + r.bytes;
        }
        Ok(())
    }
    fn find_record(&self, key: u64) -> Result<Option<Pos2Record>> {
        let mut lo = 0usize;
        let mut hi = self.records as usize;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let r = self.record_at(mid)?;
            match r.key.cmp(&key) {
                CmpOrdering::Less => lo = mid + 1,
                CmpOrdering::Equal => return Ok(Some(r)),
                CmpOrdering::Greater => hi = mid,
            }
        }
        Ok(None)
    }
    fn payload(&self, r: Pos2Record) -> Result<&[u8]> {
        let b = self.payload_offset + r.offset as usize;
        let e = b + r.bytes as usize;
        self.data()
            .get(b..e)
            .ok_or_else(|| SearchError::Format("PRPOS002 payload OOB".into()))
    }
    pub(super) fn has_exact_variable_record(&self, query: &[u8]) -> Result<bool> {
        let query = fold_ascii(query);
        if !(4..=POS2_MAX_GRAM).contains(&query.len()) {
            return Ok(false);
        }
        Ok(self.find_record(gram_key(&query))?.is_some())
    }

    fn checked_payload(&self, r: Pos2Record) -> Result<&[u8]> {
        let p = self.payload(r)?;
        if posting_checksum2(r.key, r.unit_count, r.occurrence_count, p) != r.checksum {
            return Err(SearchError::Format(
                "PRPOS002 posting checksum mismatch".into(),
            ));
        }
        Ok(p)
    }
    fn total_units(&self) -> Result<u64> {
        let mut n = 0;
        for i in 0..self.records as usize {
            n += u64::from(self.record_at(i)?.unit_count);
        }
        Ok(n)
    }
    fn total_occurrences(&self) -> Result<u64> {
        let mut n = 0;
        for i in 0..self.records as usize {
            n += u64::from(self.record_at(i)?.occurrence_count);
        }
        Ok(n)
    }
    fn posting_parts(&self, r: Pos2Record) -> Result<(&[u8], &[u8], &[u8])> {
        let p = self.checked_payload(r)?;
        if p.len() < 8 {
            return Err(SearchError::Format("PRPOS002 posting too small".into()));
        }
        let unit_bytes = rd32(p, 0)? as usize;
        let offset_count = rd32(p, 4)? as usize;
        let gram_len = (r.key >> 56) as usize;
        let expected_offsets = if gram_len == 3 {
            r.unit_count as usize + 1
        } else {
            0
        };
        if offset_count != expected_offsets {
            return Err(SearchError::Format(
                "PRPOS002 offsets count mismatch".into(),
            ));
        }
        let ub = 8;
        let ue = ub + unit_bytes;
        let ob = ue;
        let oe = ob + offset_count * 4;
        if oe > p.len() {
            return Err(SearchError::Format(
                "PRPOS002 posting layout invalid".into(),
            ));
        }
        Ok((&p[ub..ue], &p[ob..oe], &p[oe..]))
    }
    fn decode_units(&self, r: Pos2Record) -> Result<Arc<Vec<u32>>> {
        {
            let mut c = self
                .cache
                .lock()
                .map_err(|_| SearchError::Format("PRPOS002 cache poisoned".into()))?;
            if let Some(v) = c.values.get(&r.key).cloned() {
                if let Some(i) = c.order.iter().position(|&k| k == r.key) {
                    c.order.remove(i);
                }
                c.order.push_back(r.key);
                return Ok(v);
            }
        }
        let (stream, _, _) = self.posting_parts(r)?;
        let units = Arc::new(decode_varints_exact(stream, r.unit_count as usize)?);
        let mut c = self
            .cache
            .lock()
            .map_err(|_| SearchError::Format("PRPOS002 cache poisoned".into()))?;
        while c.values.len() >= POS2_UNIT_CACHE_MAX_ENTRIES {
            if let Some(k) = c.order.pop_front() {
                c.values.remove(&k);
            } else {
                break;
            }
        }
        c.values.insert(r.key, Arc::clone(&units));
        c.order.push_back(r.key);
        Ok(units)
    }
}

fn intersect_units(base: &mut Vec<u32>, other: &[u32]) {
    let mut out = Vec::with_capacity(base.len().min(other.len()));
    let (mut i, mut j) = (0, 0);
    while i < base.len() && j < other.len() {
        match base[i].cmp(&other[j]) {
            CmpOrdering::Less => i += 1,
            CmpOrdering::Greater => j += 1,
            CmpOrdering::Equal => {
                out.push(base[i]);
                i += 1;
                j += 1
            }
        }
    }
    *base = out;
}

pub(super) fn search_segment_positional2(
    segment: &SegmentReader,
    side: &Pos2Sidecar,
    query: &[u8],
) -> Result<Option<Vec<u32>>> {
    let query = fold_ascii(query);
    if query.len() < 4 {
        return Ok(None);
    }

    // Exact variable-length record: no position decode and no raw verifier.
    if query.len() <= POS2_MAX_GRAM
        && let Some(rec) = side.find_record(gram_key(&query))?
    {
        let units = side.decode_units(rec)?;
        return units_to_docs(segment, units.as_slice()).map(Some);
    }

    // For longer or unmaterialized literals, intersect up to three of the most selective
    // q4..q8 unit postings, then exact-verify only those units. Unit presence is a safe
    // superset filter even without positional alignment.
    let mut filters = Vec::<Pos2Record>::new();
    for len in (4..=POS2_MAX_GRAM.min(query.len())).rev() {
        for start in 0..=query.len() - len {
            if let Some(rec) = side.find_record(gram_key(&query[start..start + len]))? {
                filters.push(rec);
            }
        }
    }
    filters.sort_by_key(|rec| rec.unit_count);
    filters.dedup_by_key(|rec| rec.key);
    filters.truncate(3);
    if !filters.is_empty() {
        let first = side.decode_units(filters[0])?;
        let mut candidates = first.as_ref().clone();
        for &rec in filters.iter().skip(1) {
            let units = side.decode_units(rec)?;
            intersect_units(&mut candidates, units.as_slice());
            if candidates.is_empty() {
                return Ok(Some(Vec::new()));
            }
        }
        let matcher = ExactMatcher::new(&query);
        let mut matching = Vec::new();
        for unit in candidates {
            let begin = segment.u64_at(section::UNIT_TEXT_OFF, u64::from(unit))?;
            let end = segment.u64_at(section::UNIT_TEXT_OFF, u64::from(unit) + 1)?;
            let text = segment.raw_blob(section::TEXT_BLOB, begin, end)?;
            if matcher.contains(text) {
                matching.push(unit);
            }
        }
        return units_to_docs(segment, &matching).map(Some);
    }

    // No variable child is available: let the caller fall back to PRPOS001/PRSEG.
    Ok(None)
}

fn units_to_docs(segment: &SegmentReader, units: &[u32]) -> Result<Vec<u32>> {
    let mut docs = Vec::new();
    for &unit in units {
        let b = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
        let e = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
        for i in b..e {
            docs.push(segment.doc_base() + segment.u32_at(section::UNIT_DOCS, i)?);
        }
    }
    docs.sort_unstable();
    docs.dedup();
    Ok(docs)
}

pub struct Positional2Index {
    segments: Vec<SegmentReader>,
    sidecars: Vec<Pos2Sidecar>,
}
impl Positional2Index {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
        validate_manifest_ranges(docs, &manifest)?;
        let mut segments = Vec::with_capacity(manifest.len());
        let mut sidecars = Vec::with_capacity(manifest.len());
        for entry in manifest {
            let path = root.join(entry.file);
            let seg = SegmentReader::open(&path, false)?;
            let side = Pos2Sidecar::open_fast(&sidecar2_path(&path), &seg)?;
            segments.push(seg);
            sidecars.push(side);
        }
        Ok(Self { segments, sidecars })
    }
    pub fn search_content(&self, query: impl AsRef<[u8]>, workers: usize) -> Result<Vec<u32>> {
        let q = fold_ascii(query.as_ref());
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
                let q = &q;
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if i >= self.segments.len() {
                            break;
                        }
                        let r = match search_segment_positional2(
                            &self.segments[i],
                            &self.sidecars[i],
                            q,
                        ) {
                            Ok(Some(h)) => Ok(h),
                            Ok(None) => self.segments[i].search_content(q),
                            Err(e) => Err(e),
                        };
                        results.lock().expect("PRPOS002 query result poisoned")[i] = Some(r);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("PRPOS002 result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("PRPOS002 result poisoned".into()))?;
        super::merge_ordered_segment_results(results, "PRPOS002 segment not queried")
    }
}

pub fn verify_positional2_sidecars(root: impl AsRef<Path>) -> Result<()> {
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    for entry in manifest {
        let p = root.join(entry.file);
        let seg = SegmentReader::open(&p, false)?;
        let sp = sidecar2_path(&p);
        if !sp.exists() {
            return Err(SearchError::Format(format!(
                "missing PRPOS002 sidecar {}",
                sp.display()
            )));
        }
        Pos2Sidecar::open_verified(&sp, &seg)?;
    }
    Ok(())
}

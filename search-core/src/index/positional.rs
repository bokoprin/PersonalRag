use super::*;

const POS_MAGIC: &[u8; 8] = b"PRPOS001";
const POS_HEADER_BYTES: usize = 48;
const POS_PREFIX_BYTES: usize = 257 * 4;
const POS_RECORD_BYTES: usize = 24;
const POS_FOOTER_BYTES: usize = 8;
const POS_DECODE_CACHE_MAX_ENTRIES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosCodec {
    DeltaVarint = 1,
    StreamVByte = 2,
    EliasFano = 3,
    Block256Bitmap = 4,
}

impl PosCodec {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "delta" | "delta-varint" => Ok(Self::DeltaVarint),
            "streamvbyte" | "svb" => Ok(Self::StreamVByte),
            "eliasfano" | "ef" => Ok(Self::EliasFano),
            "block256" | "bitmap" => Ok(Self::Block256Bitmap),
            _ => Err(SearchError::Format(format!(
                "unknown positional codec {value}"
            ))),
        }
    }

    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::DeltaVarint => "delta",
            Self::StreamVByte => "svb",
            Self::EliasFano => "ef",
            Self::Block256Bitmap => "block256",
        }
    }

    #[must_use]
    pub const fn sidecar_extension(self) -> &'static str {
        match self {
            Self::DeltaVarint => "pos-delta",
            Self::StreamVByte => "pos-svb",
            Self::EliasFano => "pos-ef",
            Self::Block256Bitmap => "pos-block256",
        }
    }

    #[must_use]
    pub const fn production() -> Self {
        Self::EliasFano
    }

    fn from_u32(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::DeltaVarint),
            2 => Ok(Self::StreamVByte),
            3 => Ok(Self::EliasFano),
            4 => Ok(Self::Block256Bitmap),
            _ => Err(SearchError::Format("bad positional codec".into())),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PosSidecarBuildReport {
    pub segments: usize,
    pub records: u64,
    pub occurrences: u64,
    pub bytes: u64,
    pub elapsed_ms: f64,
}

#[derive(Clone, Copy, Debug)]
struct PosRecord {
    key: u32,
    count: u32,
    offset: u32,
    bytes: u32,
    checksum: u64,
}

struct PosDecodeCache {
    values: std::collections::HashMap<u32, Arc<Vec<u32>>>,
    order: std::collections::VecDeque<u32>,
}

impl PosDecodeCache {
    fn get(&mut self, key: u32) -> Option<Arc<Vec<u32>>> {
        let value = self.values.get(&key).cloned()?;
        if let Some(index) = self.order.iter().position(|&candidate| candidate == key) {
            self.order.remove(index);
        }
        self.order.push_back(key);
        Some(value)
    }

    fn insert(&mut self, key: u32, value: Arc<Vec<u32>>) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) = self.values.entry(key) {
            entry.insert(value);
            if let Some(index) = self.order.iter().position(|&candidate| candidate == key) {
                self.order.remove(index);
            }
            self.order.push_back(key);
            return;
        }
        while self.values.len() >= POS_DECODE_CACHE_MAX_ENTRIES {
            let Some(evicted) = self.order.pop_front() else {
                self.values.clear();
                break;
            };
            self.values.remove(&evicted);
        }
        self.values.insert(key, value);
        self.order.push_back(key);
    }
}

pub(super) struct PosSidecar {
    mapped: MappedFile,
    text_bytes: u32,
    codec: PosCodec,
    threshold_ppm: u32,
    records: u32,
    payload_offset: usize,
    cache: Mutex<PosDecodeCache>,
}

impl PosSidecar {
    pub(super) fn open_fast(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open_impl(path, segment, false)
    }

    pub(super) fn open_verified(path: &Path, segment: &SegmentReader) -> Result<Self> {
        Self::open_impl(path, segment, true)
    }

    fn open_impl(path: &Path, segment: &SegmentReader, verify_full: bool) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        let data = mapped.as_slice();
        if data.len() < POS_HEADER_BYTES + POS_PREFIX_BYTES + POS_FOOTER_BYTES {
            return Err(SearchError::Format("positional sidecar too small".into()));
        }
        if data.get(..8) != Some(POS_MAGIC.as_slice()) {
            return Err(SearchError::Format("bad positional sidecar magic".into()));
        }
        let unit_count = rd32(data, 8)?;
        let text_bytes = rd32(data, 12)?;
        let records = rd32(data, 16)?;
        let codec = PosCodec::from_u32(rd32(data, 20)?)?;
        let threshold_ppm = rd32(data, 24)?;
        let segment_checksum = rd64(data, 32)?;
        let payload_bytes = usize::try_from(rd64(data, 40)?)
            .map_err(|_| SearchError::Format("positional payload too large".into()))?;
        if unit_count != segment.unit_count() || segment_checksum != segment.stored_checksum()? {
            return Err(SearchError::Format(
                "positional sidecar segment mismatch".into(),
            ));
        }
        let expected_text_bytes = u32::try_from(segment.desc[section::TEXT_BLOB].size)
            .map_err(|_| SearchError::Format("segment text exceeds positional u32 range".into()))?;
        if text_bytes != expected_text_bytes {
            return Err(SearchError::Format(
                "positional sidecar text size mismatch".into(),
            ));
        }
        let records_usize = usize::try_from(records)
            .map_err(|_| SearchError::Format("positional record count too large".into()))?;
        let payload_offset = POS_HEADER_BYTES
            .checked_add(POS_PREFIX_BYTES)
            .and_then(|v| v.checked_add(records_usize.saturating_mul(POS_RECORD_BYTES)))
            .ok_or_else(|| SearchError::Format("positional layout overflow".into()))?;
        let payload_end = payload_offset
            .checked_add(payload_bytes)
            .ok_or_else(|| SearchError::Format("positional payload overflow".into()))?;
        if payload_end.checked_add(POS_FOOTER_BYTES) != Some(data.len()) {
            return Err(SearchError::Format(
                "positional sidecar length mismatch".into(),
            ));
        }
        let expected_checksum = rd64(data, payload_end)?;
        if verify_full && fnv1a(&data[..payload_end]) != expected_checksum {
            return Err(SearchError::Format(
                "positional sidecar checksum mismatch".into(),
            ));
        }
        let prefix = &data[POS_HEADER_BYTES..POS_HEADER_BYTES + POS_PREFIX_BYTES];
        let mut prev = 0u32;
        for i in 0..=256usize {
            let value = rd32(prefix, i * 4)?;
            if value < prev || value > records {
                return Err(SearchError::Format("positional prefix invalid".into()));
            }
            prev = value;
        }
        if prev != records {
            return Err(SearchError::Format(
                "positional prefix terminal mismatch".into(),
            ));
        }
        let sidecar = Self {
            mapped,
            text_bytes,
            codec,
            threshold_ppm,
            records,
            payload_offset,
            cache: Mutex::new(PosDecodeCache {
                values: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
        };
        sidecar.validate_records()?;
        Ok(sidecar)
    }

    fn data(&self) -> &[u8] {
        self.mapped.as_slice()
    }

    fn record_at(&self, index: usize, high: u8) -> Result<PosRecord> {
        if index >= self.records as usize {
            return Err(SearchError::Format("positional record OOB".into()));
        }
        let off = POS_HEADER_BYTES + POS_PREFIX_BYTES + index * POS_RECORD_BYTES;
        let suffix = u32::from(rd16(self.data(), off)?);
        Ok(PosRecord {
            key: (u32::from(high) << 16) | suffix,
            count: rd32(self.data(), off + 4)?,
            offset: rd32(self.data(), off + 8)?,
            bytes: rd32(self.data(), off + 12)?,
            checksum: rd64(self.data(), off + 16)?,
        })
    }

    fn validate_records(&self) -> Result<()> {
        let prefix = &self.data()[POS_HEADER_BYTES..POS_HEADER_BYTES + POS_PREFIX_BYTES];
        let payload_len = self.data().len() - self.payload_offset - POS_FOOTER_BYTES;
        let mut previous_key = None;
        let mut previous_end = 0u32;
        for high in 0..=255usize {
            let begin = rd32(prefix, high * 4)? as usize;
            let end = rd32(prefix, (high + 1) * 4)? as usize;
            for idx in begin..end {
                let rec = self.record_at(idx, high as u8)?;
                if previous_key.is_some_and(|key| rec.key <= key) {
                    return Err(SearchError::Format("positional records unsorted".into()));
                }
                if rec.offset < previous_end
                    || usize::try_from(u64::from(rec.offset) + u64::from(rec.bytes))
                        .ok()
                        .is_none_or(|end| end > payload_len)
                {
                    return Err(SearchError::Format(
                        "positional payload range invalid".into(),
                    ));
                }
                previous_key = Some(rec.key);
                previous_end = rec.offset.saturating_add(rec.bytes);
            }
        }
        Ok(())
    }

    fn find_record(&self, key: u32) -> Result<Option<PosRecord>> {
        let high = (key >> 16) as u8;
        let prefix = &self.data()[POS_HEADER_BYTES..POS_HEADER_BYTES + POS_PREFIX_BYTES];
        let mut lo = rd32(prefix, usize::from(high) * 4)? as usize;
        let mut hi = rd32(prefix, (usize::from(high) + 1) * 4)? as usize;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let rec = self.record_at(mid, high)?;
            match rec.key.cmp(&key) {
                CmpOrdering::Less => lo = mid + 1,
                CmpOrdering::Equal => return Ok(Some(rec)),
                CmpOrdering::Greater => hi = mid,
            }
        }
        Ok(None)
    }

    fn payload(&self, rec: PosRecord) -> Result<&[u8]> {
        let begin = self.payload_offset + rec.offset as usize;
        let end = begin + rec.bytes as usize;
        self.data()
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("positional payload OOB".into()))
    }

    fn decode_positions(&self, key: u32) -> Result<Option<Arc<Vec<u32>>>> {
        if let Some(values) = self
            .cache
            .lock()
            .map_err(|_| SearchError::Format("positional cache mutex poisoned".into()))?
            .get(key)
        {
            return Ok(Some(values));
        }
        let Some(rec) = self.find_record(key)? else {
            return Ok(None);
        };
        let payload = self.payload(rec)?;
        if posting_checksum(rec.key, rec.count, payload) != rec.checksum {
            return Err(SearchError::Format(
                "positional posting checksum mismatch".into(),
            ));
        }
        let values = Arc::new(match self.codec {
            PosCodec::DeltaVarint => decode_delta_varint(payload, rec.count)?,
            PosCodec::StreamVByte => decode_stream_vbyte(payload, rec.count)?,
            PosCodec::EliasFano => decode_elias_fano(payload, rec.count)?,
            PosCodec::Block256Bitmap => decode_block_bitmap(payload, rec.count, self.text_bytes)?,
        });
        self.cache
            .lock()
            .map_err(|_| SearchError::Format("positional cache mutex poisoned".into()))?
            .insert(key, Arc::clone(&values));
        Ok(Some(values))
    }
}

#[cold]
fn append_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cold]
fn encode_delta_varint(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u32;
    for (i, &value) in values.iter().enumerate() {
        append_varint(&mut out, if i == 0 { value } else { value - prev });
        prev = value;
    }
    out
}

#[cold]
fn decode_delta_varint(data: &[u8], expected: u32) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(expected as usize);
    let mut pos = 0usize;
    let mut prev = 0u32;
    while pos < data.len() && out.len() < expected as usize {
        let mut value = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = *data
                .get(pos)
                .ok_or_else(|| SearchError::Format("truncated positional varint".into()))?;
            pos += 1;
            if shift > 28 {
                return Err(SearchError::Format("invalid positional varint".into()));
            }
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let decoded = if out.is_empty() {
            value
        } else {
            prev.checked_add(value)
                .ok_or_else(|| SearchError::Format("positional delta overflow".into()))?
        };
        out.push(decoded);
        prev = decoded;
    }
    if pos != data.len() || out.len() != expected as usize {
        return Err(SearchError::Format(
            "positional varint count mismatch".into(),
        ));
    }
    Ok(out)
}

#[cold]
fn bytes_for_u32(value: u32) -> usize {
    if value <= 0xff {
        1
    } else if value <= 0xffff {
        2
    } else if value <= 0xff_ffff {
        3
    } else {
        4
    }
}

#[cold]
fn encode_stream_vbyte(values: &[u32]) -> Vec<u8> {
    let mut deltas = Vec::with_capacity(values.len());
    let mut prev = 0u32;
    for (i, &v) in values.iter().enumerate() {
        deltas.push(if i == 0 { v } else { v - prev });
        prev = v;
    }
    let controls_len = deltas.len().div_ceil(4);
    let mut controls = vec![0u8; controls_len];
    let mut bytes = Vec::new();
    for (i, &d) in deltas.iter().enumerate() {
        let n = bytes_for_u32(d);
        controls[i / 4] |= ((n - 1) as u8) << ((i % 4) * 2);
        let raw = d.to_le_bytes();
        bytes.extend_from_slice(&raw[..n]);
    }
    let mut out = Vec::with_capacity(4 + controls.len() + bytes.len());
    out.extend_from_slice(&(controls_len as u32).to_le_bytes());
    out.extend_from_slice(&controls);
    out.extend_from_slice(&bytes);
    out
}

#[cold]
fn decode_stream_vbyte(data: &[u8], expected: u32) -> Result<Vec<u32>> {
    if data.len() < 4 {
        return Err(SearchError::Format("truncated streamvbyte".into()));
    }
    let controls_len = rd32(data, 0)? as usize;
    if data.len() < 4 + controls_len {
        return Err(SearchError::Format("bad streamvbyte controls".into()));
    }
    let controls = &data[4..4 + controls_len];
    let mut cursor = 4 + controls_len;
    let mut out = Vec::with_capacity(expected as usize);
    let mut prev = 0u32;
    for i in 0..expected as usize {
        let control = *controls
            .get(i / 4)
            .ok_or_else(|| SearchError::Format("streamvbyte control OOB".into()))?;
        let n = usize::from(((control >> ((i % 4) * 2)) & 3) + 1);
        let bytes = data
            .get(cursor..cursor + n)
            .ok_or_else(|| SearchError::Format("streamvbyte data OOB".into()))?;
        cursor += n;
        let mut raw = [0u8; 4];
        raw[..n].copy_from_slice(bytes);
        let d = u32::from_le_bytes(raw);
        let v = if i == 0 {
            d
        } else {
            prev.checked_add(d)
                .ok_or_else(|| SearchError::Format("streamvbyte overflow".into()))?
        };
        out.push(v);
        prev = v;
    }
    if cursor != data.len() {
        return Err(SearchError::Format("streamvbyte trailing bytes".into()));
    }
    Ok(out)
}

#[cold]
fn set_packed_bits(out: &mut [u8], bit_offset: usize, width: u8, value: u32) {
    for bit in 0..usize::from(width) {
        if (value >> bit) & 1 != 0 {
            let p = bit_offset + bit;
            out[p / 8] |= 1 << (p % 8);
        }
    }
}
#[inline(always)]
fn get_packed_bits(data: &[u8], bit_offset: usize, width: u8) -> u32 {
    if width == 0 {
        return 0;
    }
    let byte_offset = bit_offset / 8;
    let shift = bit_offset & 7;
    let word = if byte_offset + 8 <= data.len() {
        // SAFETY: the branch above proves that eight bytes are readable.
        unsafe { std::ptr::read_unaligned(data.as_ptr().add(byte_offset).cast::<u64>()) }
    } else {
        let mut raw = [0u8; 8];
        let tail = &data[byte_offset..];
        raw[..tail.len()].copy_from_slice(tail);
        u64::from_le_bytes(raw)
    };
    let mask = (1u64 << width) - 1;
    ((u64::from_le(word) >> shift) & mask) as u32
}

fn encode_elias_fano(values: &[u32]) -> Vec<u8> {
    if values.is_empty() {
        return vec![0; 12];
    }
    let n = values.len() as u64;
    let universe = u64::from(values.last().copied().unwrap_or(0)) + 1;
    let ratio = (universe / n).max(1);
    let l = (63 - ratio.leading_zeros() as u64).min(31) as u8;
    let low_bits = values.len() * usize::from(l);
    let low_bytes = low_bits.div_ceil(8);
    let high_max = (u64::from(*values.last().unwrap()) >> l) + n + 1;
    let high_bytes = (high_max as usize).div_ceil(8);
    let mut low = vec![0u8; low_bytes];
    let mut high = vec![0u8; high_bytes];
    let mask = if l == 0 { 0 } else { (1u32 << l) - 1 };
    for (i, &v) in values.iter().enumerate() {
        if l != 0 {
            set_packed_bits(&mut low, i * usize::from(l), l, v & mask);
        }
        let p = ((u64::from(v) >> l) + i as u64) as usize;
        high[p / 8] |= 1 << (p % 8);
    }
    let mut out = Vec::with_capacity(12 + low.len() + high.len());
    out.push(l);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&(low.len() as u32).to_le_bytes());
    out.extend_from_slice(&(high.len() as u32).to_le_bytes());
    out.extend_from_slice(&low);
    out.extend_from_slice(&high);
    out
}

#[inline(never)]
fn decode_elias_fano(data: &[u8], expected: u32) -> Result<Vec<u32>> {
    if data.len() < 12 {
        return Err(SearchError::Format("truncated elias-fano".into()));
    }
    let l = data[0];
    if l > 31 {
        return Err(SearchError::Format("bad elias-fano low bits".into()));
    }
    let low_bytes = rd32(data, 4)? as usize;
    let high_bytes = rd32(data, 8)? as usize;
    if 12usize
        .checked_add(low_bytes)
        .and_then(|x| x.checked_add(high_bytes))
        != Some(data.len())
    {
        return Err(SearchError::Format("bad elias-fano lengths".into()));
    }
    let low = &data[12..12 + low_bytes];
    let high = &data[12 + low_bytes..];
    let mut out = Vec::with_capacity(expected as usize);
    let mut seen = 0usize;
    let mut byte_base = 0usize;
    for chunk in high.chunks(8) {
        let mut word_bytes = [0u8; 8];
        word_bytes[..chunk.len()].copy_from_slice(chunk);
        let mut bits = u64::from_le_bytes(word_bytes);
        while bits != 0 {
            let bit_in_word = bits.trailing_zeros() as usize;
            let bitpos = byte_base * 8 + bit_in_word;
            let upper = bitpos
                .checked_sub(seen)
                .ok_or_else(|| SearchError::Format("elias-fano select underflow".into()))?
                as u32;
            let lower = if l == 0 {
                0
            } else {
                get_packed_bits(low, seen * usize::from(l), l)
            };
            out.push((upper << l) | lower);
            seen += 1;
            if seen == expected as usize {
                break;
            }
            bits &= bits - 1;
        }
        if seen == expected as usize {
            break;
        }
        byte_base += chunk.len();
    }
    if out.len() != expected as usize {
        return Err(SearchError::Format("elias-fano count mismatch".into()));
    }
    Ok(out)
}

#[cold]
fn encode_block_bitmap(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < values.len() {
        let block = values[i] / 256;
        out.extend_from_slice(&block.to_le_bytes());
        let mask_start = out.len();
        out.resize(mask_start + 32, 0);
        while i < values.len() && values[i] / 256 == block {
            let bit = values[i] & 255;
            out[mask_start + (bit / 8) as usize] |= 1 << (bit % 8);
            i += 1;
        }
    }
    out
}

#[cold]
fn decode_block_bitmap(data: &[u8], expected: u32, universe: u32) -> Result<Vec<u32>> {
    if !data.len().is_multiple_of(36) {
        return Err(SearchError::Format("bad positional block bitmap".into()));
    }
    let mut out = Vec::with_capacity(expected as usize);
    for chunk in data.chunks_exact(36) {
        let block = rd32(chunk, 0)?;
        for byte_index in 0..32usize {
            let mut bits = chunk[4 + byte_index];
            while bits != 0 {
                let bit = bits.trailing_zeros();
                let value = block * 256 + byte_index as u32 * 8 + bit;
                if value < universe {
                    out.push(value);
                }
                bits &= bits - 1;
            }
        }
    }
    if out.len() != expected as usize {
        return Err(SearchError::Format("block bitmap count mismatch".into()));
    }
    Ok(out)
}

#[inline(never)]
fn posting_checksum(key: u32, count: u32, payload: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in key
        .to_le_bytes()
        .into_iter()
        .chain(count.to_le_bytes())
        .chain(payload.iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cold]
fn encode_positions(values: &[u32], codec: PosCodec) -> Vec<u8> {
    match codec {
        PosCodec::DeltaVarint => encode_delta_varint(values),
        PosCodec::StreamVByte => encode_stream_vbyte(values),
        PosCodec::EliasFano => encode_elias_fano(values),
        PosCodec::Block256Bitmap => encode_block_bitmap(values),
    }
}

#[cold]
pub(super) fn dense_q3_keys<S: SegmentBuildSource>(
    segment: &S,
    threshold_ppm: u32,
) -> Result<Vec<u32>> {
    let directory = segment.build_q3_directory()?;
    if directory.len() < 257 * 4 {
        return Err(SearchError::Format("bad q3 directory".into()));
    }
    let entries = rd32(directory, 256 * 4)? as usize;
    if directory.len() != POS_PREFIX_BYTES + entries * 10 {
        return Err(SearchError::Format("bad q3 directory shape".into()));
    }
    let minimum = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(threshold_ppm))
        .div_ceil(1_000_000);
    let mut keys = Vec::new();
    for high in 0..256usize {
        let begin = rd32(directory, high * 4)? as usize;
        let end = rd32(directory, (high + 1) * 4)? as usize;
        for idx in begin..end {
            let off = POS_PREFIX_BYTES + idx * 10;
            let suffix = u32::from(rd16(directory, off)?);
            let count = rd32(directory, off + 2)? & 0x3fff_ffff;
            if u64::from(count) >= minimum {
                keys.push((high as u32) << 16 | suffix);
            }
        }
    }
    Ok(keys)
}

#[cold]
pub(super) fn serialize_segment_sidecar_from_positions<S: SegmentBuildSource>(
    segment: &S,
    codec: PosCodec,
    threshold_ppm: u32,
    keys: &[u32],
    positions: &[Vec<u32>],
) -> Result<Vec<u8>> {
    if keys.len() != positions.len() {
        return Err(SearchError::Format(
            "positional key/position shape mismatch".into(),
        ));
    }
    let text_blob = segment.build_text_blob()?;
    let text_bytes = u32::try_from(text_blob.len())
        .map_err(|_| SearchError::Format("segment text exceeds positional u32 range".into()))?;
    let mut records = Vec::<PosRecord>::with_capacity(keys.len());
    let mut payload = Vec::new();
    for (&key, values) in keys.iter().zip(positions) {
        let encoded = encode_positions(values, codec);
        let offset = u32::try_from(payload.len())
            .map_err(|_| SearchError::Format("positional payload >4GiB".into()))?;
        let bytes = u32::try_from(encoded.len())
            .map_err(|_| SearchError::Format("positional record >4GiB".into()))?;
        let count = u32::try_from(values.len())
            .map_err(|_| SearchError::Format("too many positional occurrences".into()))?;
        records.push(PosRecord {
            key,
            count,
            offset,
            bytes,
            checksum: posting_checksum(key, count, &encoded),
        });
        payload.extend_from_slice(&encoded);
    }
    let mut prefix = [0u32; 257];
    let mut cursor = 0usize;
    for (high, slot) in prefix.iter_mut().enumerate().take(256) {
        *slot = cursor as u32;
        while cursor < records.len() && ((records[cursor].key >> 16) as usize) == high {
            cursor += 1;
        }
    }
    prefix[256] = records.len() as u32;
    let mut out = Vec::with_capacity(
        POS_HEADER_BYTES
            + POS_PREFIX_BYTES
            + records.len() * POS_RECORD_BYTES
            + payload.len()
            + POS_FOOTER_BYTES,
    );
    out.extend_from_slice(POS_MAGIC);
    out.extend_from_slice(&segment.build_unit_count().to_le_bytes());
    out.extend_from_slice(&text_bytes.to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    out.extend_from_slice(&(codec as u32).to_le_bytes());
    out.extend_from_slice(&threshold_ppm.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&segment.build_segment_checksum()?.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    for p in prefix {
        out.extend_from_slice(&p.to_le_bytes());
    }
    for rec in &records {
        out.extend_from_slice(&(rec.key as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&rec.count.to_le_bytes());
        out.extend_from_slice(&rec.offset.to_le_bytes());
        out.extend_from_slice(&rec.bytes.to_le_bytes());
        out.extend_from_slice(&rec.checksum.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    let checksum = fnv1a(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

#[cold]
pub(super) fn build_segment_sidecar<S: SegmentBuildSource>(
    segment: &S,
    codec: PosCodec,
    threshold_ppm: u32,
) -> Result<Vec<u8>> {
    if threshold_ppm > 1_000_000 {
        return Err(SearchError::Format(
            "positional density threshold > 100%".into(),
        ));
    }
    let text_blob = segment.build_text_blob()?;
    let keys = dense_q3_keys(segment, threshold_ppm)?;
    let mut key_to_index = std::collections::HashMap::<u32, usize>::with_capacity(keys.len());
    for (i, &key) in keys.iter().enumerate() {
        key_to_index.insert(key, i);
    }
    let mut positions = vec![Vec::<u32>::new(); keys.len()];
    for unit in 0..segment.build_unit_count() {
        let (begin, end) = segment.build_unit_text_bounds(unit)?;
        let text = text_blob
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("positional unit text OOB".into()))?;
        let base = u32::try_from(begin)
            .map_err(|_| SearchError::Format("text offset exceeds u32".into()))?;
        for (offset, gram) in text.windows(3).enumerate() {
            let key = k3(gram[0], gram[1], gram[2]);
            if let Some(&slot) = key_to_index.get(&key) {
                positions[slot].push(
                    base + u32::try_from(offset)
                        .map_err(|_| SearchError::Format("position exceeds u32".into()))?,
                );
            }
        }
    }
    serialize_segment_sidecar_from_positions(segment, codec, threshold_ppm, &keys, &positions)
}

fn sidecar_path(segment_path: &Path, codec: PosCodec) -> PathBuf {
    segment_path.with_extension(format!("pos-{}", codec.tag()))
}

#[cold]
fn sidecar_report_values(sidecar: &PosSidecar, path: &Path) -> Result<(u64, u64, u64)> {
    let prefix = &sidecar.data()[POS_HEADER_BYTES..POS_HEADER_BYTES + POS_PREFIX_BYTES];
    let mut occurrences = 0u64;
    for high in 0..256usize {
        let begin = rd32(prefix, high * 4)? as usize;
        let end = rd32(prefix, (high + 1) * 4)? as usize;
        for idx in begin..end {
            occurrences = occurrences
                .checked_add(u64::from(sidecar.record_at(idx, high as u8)?.count))
                .ok_or_else(|| {
                    SearchError::Format("positional occurrence report overflow".into())
                })?;
        }
    }
    Ok((
        fs::metadata(path)?.len(),
        u64::from(sidecar.records),
        occurrences,
    ))
}

#[cold]
fn build_one_positional_sidecar(
    root: &Path,
    entry: &ManifestSegment,
    segment_index: usize,
    codec: PosCodec,
    threshold_ppm: u32,
    durable: bool,
) -> Result<(u64, u64, u64)> {
    let segment_path = root.join(&entry.file);
    let segment = SegmentReader::open(&segment_path, false)?;
    let path = sidecar_path(&segment_path, codec);
    if path.exists() {
        let sidecar = PosSidecar::open_verified(&path, &segment)?;
        if sidecar.codec != codec || sidecar.threshold_ppm != threshold_ppm {
            return Err(SearchError::Format(
                "existing positional sidecar configuration mismatch".into(),
            ));
        }
        return sidecar_report_values(&sidecar, &path);
    }

    let bytes = build_segment_sidecar(&segment, codec, threshold_ppm)?;
    let temp = path.with_extension(format!(
        "pos-{}.tmp-{}-{segment_index}",
        codec.tag(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let publish = (|| -> Result<()> {
        file.write_all(&bytes)?;
        if durable {
            file.sync_all()?;
        }
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o444))?;
        }
        fs::rename(&temp, &path)?;
        if durable {
            #[cfg(unix)]
            fs::File::open(root)?.sync_all()?;
        }
        Ok(())
    })();
    if publish.is_err() {
        let _ = fs::remove_file(&temp);
    }
    publish?;
    let sidecar = PosSidecar::open_verified(&path, &segment)?;
    sidecar_report_values(&sidecar, &path)
}

pub fn build_positional_sidecars(
    root: impl AsRef<Path>,
    codec: PosCodec,
    threshold_ppm: u32,
    durable: bool,
) -> Result<PosSidecarBuildReport> {
    let started = Instant::now();
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    if manifest.is_empty() {
        return Ok(PosSidecarBuildReport::default());
    }
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min(manifest.len());
    let next = AtomicUsize::new(0);
    type BuildResult = Option<Result<(u64, u64, u64)>>;
    let results: Arc<Mutex<Vec<BuildResult>>> = Arc::new(Mutex::new(
        std::iter::repeat_with(|| None)
            .take(manifest.len())
            .collect(),
    ));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let results = Arc::clone(&results);
            let manifest = &manifest;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if index >= manifest.len() {
                        break;
                    }
                    let result = build_one_positional_sidecar(
                        root,
                        &manifest[index],
                        index,
                        codec,
                        threshold_ppm,
                        durable,
                    );
                    results
                        .lock()
                        .expect("positional build result mutex poisoned")[index] = Some(result);
                }
            });
        }
    });

    let mut report = PosSidecarBuildReport::default();
    for result in Arc::try_unwrap(results)
        .map_err(|_| SearchError::Format("positional build result ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("positional build result mutex poisoned".into()))?
    {
        let (bytes, records, occurrences) = result
            .ok_or_else(|| SearchError::Format("positional segment was not built".into()))??;
        report.segments += 1;
        report.bytes = report
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| SearchError::Format("positional byte report overflow".into()))?;
        report.records = report
            .records
            .checked_add(records)
            .ok_or_else(|| SearchError::Format("positional record report overflow".into()))?;
        report.occurrences = report
            .occurrences
            .checked_add(occurrences)
            .ok_or_else(|| SearchError::Format("positional occurrence report overflow".into()))?;
    }
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

fn covering_offsets(query_len: usize) -> Vec<usize> {
    if query_len < 4 {
        return Vec::new();
    }
    let mut offsets = vec![0usize];
    let mut covered = 3usize;
    while covered < query_len {
        let next = covered.min(query_len - 3);
        if offsets.last().copied() != Some(next) {
            offsets.push(next);
        }
        covered = next + 3;
    }
    offsets
}

fn intersect_shifted(base: &mut Vec<u32>, other: &[u32], shift: u32) {
    let mut out = Vec::with_capacity(base.len().min(other.len()));
    let mut i = 0usize;
    let mut j = 0usize;
    while i < base.len() && j < other.len() {
        let Some(other_start) = other[j].checked_sub(shift) else {
            j += 1;
            continue;
        };
        match base[i].cmp(&other_start) {
            CmpOrdering::Less => i += 1,
            CmpOrdering::Greater => j += 1,
            CmpOrdering::Equal => {
                out.push(base[i]);
                i += 1;
                j += 1;
            }
        }
    }
    *base = out;
}

#[inline(never)]
pub(super) fn search_segment_positional(
    segment: &SegmentReader,
    sidecar: &PosSidecar,
    query: &[u8],
) -> Result<Option<Vec<u32>>> {
    let query = fold_ascii(query);
    if query.len() < 4 {
        return Ok(None);
    }
    let offsets = covering_offsets(query.len());
    if offsets.is_empty() {
        return Ok(None);
    }
    let mut lists = Vec::<(usize, Arc<Vec<u32>>)>::with_capacity(offsets.len());
    for &offset in &offsets {
        let gram = &query[offset..offset + 3];
        let Some(positions) = sidecar.decode_positions(k3(gram[0], gram[1], gram[2]))? else {
            return Ok(None);
        };
        lists.push((offset, positions));
    }
    lists.sort_by_key(|(_, list)| list.len());
    let (first_offset, first_positions) = lists.remove(0);
    let mut starts: Vec<u32> = first_positions
        .iter()
        .copied()
        .filter_map(|p| p.checked_sub(first_offset as u32))
        .collect();
    for (offset, positions) in lists {
        intersect_shifted(&mut starts, positions.as_slice(), offset as u32);
        if starts.is_empty() {
            break;
        }
    }
    let unit_offsets = segment.section(section::UNIT_TEXT_OFF)?;
    let mut units = Vec::<u32>::new();
    let mut unit = 0usize;
    let mut begin = if segment.unit_count() == 0 {
        0
    } else {
        rd64(unit_offsets, 0)?
    };
    let mut end = if segment.unit_count() == 0 {
        0
    } else {
        rd64(unit_offsets, 8)?
    };
    for start in starts {
        let start64 = u64::from(start);
        while unit < segment.unit_count() as usize && start64 >= end {
            unit += 1;
            if unit >= segment.unit_count() as usize {
                break;
            }
            begin = end;
            end = rd64(unit_offsets, (unit + 1) * 8)?;
        }
        if unit >= segment.unit_count() as usize {
            break;
        }
        if start64 >= begin
            && start64 + query.len() as u64 <= end
            && units.last().copied() != Some(unit as u32)
        {
            units.push(unit as u32);
        }
    }
    let mut docs = Vec::new();
    for unit in units {
        let begin = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
        let end = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
        for i in begin..end {
            docs.push(segment.doc_base() + segment.u32_at(section::UNIT_DOCS, i)?);
        }
    }
    docs.sort_unstable();
    docs.dedup();
    Ok(Some(docs))
}

type PosSegmentResult = Option<Result<Vec<u32>>>;

pub struct PositionalIndex {
    segments: Vec<SegmentReader>,
    sidecars: Vec<PosSidecar>,
}

impl PositionalIndex {
    pub fn open(root: impl AsRef<Path>, codec: PosCodec) -> Result<Self> {
        let root = root.as_ref();
        let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
        validate_manifest_ranges(docs, &manifest)?;
        let mut segments = Vec::with_capacity(manifest.len());
        let mut sidecars = Vec::with_capacity(manifest.len());
        for entry in manifest {
            let path = root.join(entry.file);
            let segment = SegmentReader::open(&path, false)?;
            let sidecar = PosSidecar::open_fast(&sidecar_path(&path, codec), &segment)?;
            if sidecar.codec != codec {
                return Err(SearchError::Format("positional codec mismatch".into()));
            }
            segments.push(segment);
            sidecars.push(sidecar);
        }
        Ok(Self { segments, sidecars })
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>, workers: usize) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        if query.len() < 4 {
            return Err(SearchError::Format(
                "positional search requires >=4 bytes".into(),
            ));
        }
        let workers = workers.max(1).min(self.segments.len().max(1));
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<PosSegmentResult>>> = Arc::new(Mutex::new(
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
                        let idx = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if idx >= self.segments.len() {
                            break;
                        }
                        let result = match search_segment_positional(
                            &self.segments[idx],
                            &self.sidecars[idx],
                            query,
                        ) {
                            Ok(Some(hits)) => Ok(hits),
                            Ok(None) => self.segments[idx].search_content(query),
                            Err(e) => Err(e),
                        };
                        results.lock().expect("positional result mutex poisoned")[idx] =
                            Some(result);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("positional result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("positional result mutex poisoned".into()))?;
        super::merge_ordered_segment_results(results, "positional segment not searched")
    }
}

pub fn verify_positional_sidecars(root: impl AsRef<Path>, codec: PosCodec) -> Result<()> {
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    for entry in manifest {
        let segment_path = root.join(entry.file);
        let segment = SegmentReader::open(&segment_path, false)?;
        let path = sidecar_path(&segment_path, codec);
        if !path.exists() {
            return Err(SearchError::Format(format!(
                "missing positional sidecar {}",
                path.display()
            )));
        }
        let sidecar = PosSidecar::open_verified(&path, &segment)?;
        if sidecar.codec != codec {
            return Err(SearchError::Format("positional codec mismatch".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_decode_cache_is_bounded_lru() {
        let mut cache = PosDecodeCache {
            values: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
        };
        for key in 0..=POS_DECODE_CACHE_MAX_ENTRIES as u32 {
            cache.insert(key, Arc::new(vec![key]));
        }
        assert_eq!(cache.values.len(), POS_DECODE_CACHE_MAX_ENTRIES);
        assert!(!cache.values.contains_key(&0));
        assert!(
            cache
                .values
                .contains_key(&(POS_DECODE_CACHE_MAX_ENTRIES as u32))
        );

        let oldest_remaining = 1u32;
        assert!(cache.get(oldest_remaining).is_some());
        cache.insert(10_000, Arc::new(vec![10_000]));
        assert!(cache.values.contains_key(&oldest_remaining));
        assert!(!cache.values.contains_key(&2));
    }
}

from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_shard = '''            let ids = shard[begin..position]
                .iter()
                .map(|packed| packed & 0xffff)
                .collect::<Vec<_>>();
            let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
            let count = u32::try_from(ids.len())
                .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
new_shard = '''            let posting_len = position - begin;
            let (encoding, offset, bytes) = encode_q3_indexed(
                posting_len,
                universe,
                &mut q3blob,
                |index| shard[begin + index] & 0xffff,
            )?;
            let count = u32::try_from(posting_len)
                .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
if text.count(old_shard) != 1:
    raise SystemExit(f"packed shard ids block: expected 1 match, got {text.count(old_shard)}")
text = text.replace(old_shard, new_shard, 1)

old_pairs = '''        let ids = q3_pairs[begin..position]
            .iter()
            .map(|pair| *pair as u32)
            .collect::<Vec<_>>();
        let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
        let count = u32::try_from(ids.len())
            .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
new_pairs = '''        let posting_len = position - begin;
        let (encoding, offset, bytes) = encode_q3_indexed(
            posting_len,
            universe,
            &mut q3blob,
            |index| q3_pairs[begin + index] as u32,
        )?;
        let count = u32::try_from(posting_len)
            .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
if text.count(old_pairs) != 1:
    raise SystemExit(f"pair ids block: expected 1 match, got {text.count(old_pairs)}")
text = text.replace(old_pairs, new_pairs, 1)

start = text.index('fn encode_q3(ids: &[u32], universe: u32, blob: &mut Vec<u8>) -> Result<(Q3Encoding, u32, u32)> {')
end = text.index('\nfn compact_q3_directory', start)
new_encode = r'''fn encode_q3_indexed<F>(
    len: usize,
    universe: u32,
    blob: &mut Vec<u8>,
    id_at: F,
) -> Result<(Q3Encoding, u32, u32)>
where
    F: Fn(usize) -> u32,
{
    let offset = u32::try_from(blob.len())
        .map_err(|_| SearchError::Format("q3 payload exceeds 4GiB".into()))?;

    if len <= 32 {
        for index in 0..len {
            put_u32(blob, id_at(index));
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        return Ok((Q3Encoding::InlineU32, offset, bytes));
    }

    let density = if universe == 0 {
        0.0
    } else {
        len as f64 / f64::from(universe)
    };
    if density >= 0.20 {
        let bytes = usize::try_from(u64::from(universe).div_ceil(8))
            .map_err(|_| SearchError::Format("dense bitset too large".into()))?;
        let mask_offset = blob.len();
        blob.resize(mask_offset + bytes, 0);
        for index in 0..len {
            let id = id_at(index);
            blob[mask_offset + (id / 8) as usize] |= 1u8 << (id % 8);
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        return Ok((Q3Encoding::DenseBitset, offset, bytes));
    }

    let mut delta_bytes = 0usize;
    let mut previous = 0u32;
    for index in 0..len {
        let id = id_at(index);
        let delta = if index == 0 { id } else { id - previous };
        delta_bytes += varint_size(delta);
        previous = id;
    }
    let mut blocks = 0usize;
    let mut last_block = None;
    for index in 0..len {
        let block = id_at(index) / 256;
        if last_block != Some(block) {
            blocks += 1;
            last_block = Some(block);
        }
    }
    let block_bytes = blocks * 36;
    if block_bytes * 4 <= delta_bytes * 5 {
        let mut index = 0usize;
        while index < len {
            let block = id_at(index) / 256;
            put_u32(blob, block);
            let mask_offset = blob.len();
            blob.resize(mask_offset + 32, 0);
            while index < len && id_at(index) / 256 == block {
                let bit = id_at(index) & 255;
                blob[mask_offset + (bit / 8) as usize] |= 1u8 << (bit % 8);
                index += 1;
            }
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        Ok((Q3Encoding::Block256Bitmap, offset, bytes))
    } else {
        let mut previous = 0u32;
        for index in 0..len {
            let id = id_at(index);
            append_varint(blob, if index == 0 { id } else { id - previous });
            previous = id;
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        Ok((Q3Encoding::DeltaVarint, offset, bytes))
    }
}
'''
text = text[:start] + new_encode + text[end:]
path.write_text(text, encoding="utf-8")
print("q3 no-collect transform applied")

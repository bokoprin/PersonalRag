from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")
start = text.index("fn encode_q3(ids: &[u32], universe: u32, blob: &mut Vec<u8>) -> Result<(Q3Encoding, u32, u32)> {")
end = text.index("\nfn compact_q3_directory", start)
old = text[start:end]
new = r'''fn encode_q3(ids: &[u32], universe: u32, blob: &mut Vec<u8>) -> Result<(Q3Encoding, u32, u32)> {
    let offset = u32::try_from(blob.len())
        .map_err(|_| SearchError::Format("q3 payload exceeds 4GiB".into()))?;

    // Selection priority is Inline -> Dense -> Block/Delta. Inline and Dense therefore do not
    // need the delta-size and block-count analysis used only to choose between the sparse codecs.
    if ids.len() <= 32 {
        for &id in ids {
            put_u32(blob, id);
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        return Ok((Q3Encoding::InlineU32, offset, bytes));
    }

    let density = if universe == 0 {
        0.0
    } else {
        ids.len() as f64 / f64::from(universe)
    };
    if density >= 0.20 {
        let bytes = usize::try_from(u64::from(universe).div_ceil(8))
            .map_err(|_| SearchError::Format("dense bitset too large".into()))?;
        let mask_offset = blob.len();
        blob.resize(mask_offset + bytes, 0);
        for &id in ids {
            blob[mask_offset + (id / 8) as usize] |= 1u8 << (id % 8);
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        return Ok((Q3Encoding::DenseBitset, offset, bytes));
    }

    let mut delta_bytes = 0usize;
    let mut previous = 0u32;
    for (index, &id) in ids.iter().enumerate() {
        let delta = if index == 0 { id } else { id - previous };
        delta_bytes += varint_size(delta);
        previous = id;
    }
    let mut blocks = 0usize;
    let mut last_block = None;
    for &id in ids {
        let block = id / 256;
        if last_block != Some(block) {
            blocks += 1;
            last_block = Some(block);
        }
    }
    let block_bytes = blocks * 36;
    if block_bytes * 4 <= delta_bytes * 5 {
        let mut index = 0usize;
        while index < ids.len() {
            let block = ids[index] / 256;
            put_u32(blob, block);
            let mask_offset = blob.len();
            blob.resize(mask_offset + 32, 0);
            while index < ids.len() && ids[index] / 256 == block {
                let bit = ids[index] & 255;
                blob[mask_offset + (bit / 8) as usize] |= 1u8 << (bit % 8);
                index += 1;
            }
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        Ok((Q3Encoding::Block256Bitmap, offset, bytes))
    } else {
        let mut previous = 0u32;
        for (index, &id) in ids.iter().enumerate() {
            append_varint(blob, if index == 0 { id } else { id - previous });
            previous = id;
        }
        let bytes = u32::try_from(blob.len() - offset as usize)
            .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
        Ok((Q3Encoding::DeltaVarint, offset, bytes))
    }
}
'''
path.write_text(text[:start] + new + text[end:], encoding="utf-8")
print("encode_q3 early decision transform applied")

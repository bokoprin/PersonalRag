use crate::vnext_q3::{VNextQ3Posting, VNextQ3PostingEncoding};

pub(crate) fn intersect_direct_q3_postings(
    primary: VNextQ3Posting<'_>,
    extra: &[VNextQ3Posting<'_>],
) -> Vec<u16> {
    let Some((&first, rest)) = extra.split_first() else {
        return primary.iter().collect();
    };
    let mut current = intersect_two_direct_q3_postings(primary, first);
    for posting in rest {
        if current.is_empty() {
            break;
        }
        current = filter_direct_candidates(current, *posting);
    }
    current
}

fn intersect_two_direct_q3_postings(
    left: VNextQ3Posting<'_>,
    right: VNextQ3Posting<'_>,
) -> Vec<u16> {
    use VNextQ3PostingEncoding::{DenseBitmap, Empty, RawU16, Singleton};

    match (left.encoding(), right.encoding()) {
        (Empty, _) | (_, Empty) => Vec::new(),
        (Singleton, _) => left
            .singleton_value()
            .filter(|value| right.contains(*value))
            .into_iter()
            .collect(),
        (_, Singleton) => right
            .singleton_value()
            .filter(|value| left.contains(*value))
            .into_iter()
            .collect(),
        (RawU16, RawU16) => intersect_raw_raw(
            left.raw_u16_bytes().expect("raw posting bytes"),
            left.len(),
            right.raw_u16_bytes().expect("raw posting bytes"),
            right.len(),
        ),
        (RawU16, DenseBitmap) => intersect_raw_dense(
            left.raw_u16_bytes().expect("raw posting bytes"),
            left.len(),
            right.dense_bitmap_bytes().expect("dense posting bytes"),
            right.block_count(),
        ),
        (DenseBitmap, RawU16) => intersect_raw_dense(
            right.raw_u16_bytes().expect("raw posting bytes"),
            right.len(),
            left.dense_bitmap_bytes().expect("dense posting bytes"),
            left.block_count(),
        ),
        (DenseBitmap, DenseBitmap) => intersect_dense_dense(
            left.dense_bitmap_bytes().expect("dense posting bytes"),
            left.block_count(),
            right.dense_bitmap_bytes().expect("dense posting bytes"),
            right.block_count(),
        ),
    }
}

fn filter_direct_candidates(mut candidates: Vec<u16>, posting: VNextQ3Posting<'_>) -> Vec<u16> {
    match posting.encoding() {
        VNextQ3PostingEncoding::Empty => Vec::new(),
        VNextQ3PostingEncoding::Singleton => {
            let Some(value) = posting.singleton_value() else {
                return Vec::new();
            };
            if candidates.binary_search(&value).is_ok() {
                vec![value]
            } else {
                Vec::new()
            }
        }
        VNextQ3PostingEncoding::DenseBitmap => {
            let bitmap = posting.dense_bitmap_bytes().expect("dense posting bytes");
            let block_count = posting.block_count();
            candidates.retain(|&value| bitmap_contains(bitmap, block_count, value));
            candidates
        }
        VNextQ3PostingEncoding::RawU16 => intersect_u16_raw(
            &candidates,
            posting.raw_u16_bytes().expect("raw posting bytes"),
            posting.len(),
        ),
    }
}

#[inline]
fn raw_value(bytes: &[u8], index: usize) -> u16 {
    let off = index * 2;
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn intersect_raw_raw(left: &[u8], left_len: usize, right: &[u8], right_len: usize) -> Vec<u16> {
    if left_len == 0 || right_len == 0 {
        return Vec::new();
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") && left_len.min(right_len) >= 16 {
        // SAFETY: runtime detection guarantees AVX2. Posting byte lengths are validated by the
        // segment reader before these views are constructed.
        return unsafe { intersect_raw_raw_avx2(left, left_len, right, right_len) };
    }
    intersect_raw_raw_scalar(left, left_len, right, right_len)
}

fn intersect_raw_raw_scalar(
    left: &[u8],
    left_len: usize,
    right: &[u8],
    right_len: usize,
) -> Vec<u16> {
    let mut out = Vec::with_capacity(left_len.min(right_len));
    let mut left_index = 0usize;
    let mut right_index = 0usize;
    while left_index < left_len && right_index < right_len {
        let left_value = raw_value(left, left_index);
        let right_value = raw_value(right, right_index);
        match left_value.cmp(&right_value) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                out.push(left_value);
                left_index += 1;
                right_index += 1;
            }
        }
    }
    out
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn intersect_raw_raw_avx2(
    left: &[u8],
    left_len: usize,
    right: &[u8],
    right_len: usize,
) -> Vec<u16> {
    use std::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi16, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi16,
    };

    let (small, small_len, large, large_len) = if left_len <= right_len {
        (left, left_len, right, right_len)
    } else {
        (right, right_len, left, left_len)
    };
    let mut out = Vec::with_capacity(small_len);
    let mut large_index = 0usize;
    for small_index in 0..small_len {
        let target = raw_value(small, small_index);
        while large_index + 16 <= large_len && raw_value(large, large_index + 15) < target {
            large_index += 16;
        }
        if large_index >= large_len {
            break;
        }
        if large_index + 16 <= large_len {
            if raw_value(large, large_index) > target {
                continue;
            }
            let target_vector = _mm256_set1_epi16(target as i16);
            // SAFETY: the 16-element bound above proves the 32-byte unaligned load fits.
            let values = unsafe {
                _mm256_loadu_si256(large.as_ptr().add(large_index * 2).cast::<__m256i>())
            };
            if _mm256_movemask_epi8(_mm256_cmpeq_epi16(values, target_vector)) != 0 {
                out.push(target);
            }
            continue;
        }

        while large_index < large_len {
            let value = raw_value(large, large_index);
            if value < target {
                large_index += 1;
                continue;
            }
            if value == target {
                out.push(target);
            }
            break;
        }
    }
    out
}

fn intersect_u16_raw(candidates: &[u16], raw: &[u8], raw_len: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(candidates.len().min(raw_len));
    let mut candidate_index = 0usize;
    let mut raw_index = 0usize;
    while candidate_index < candidates.len() && raw_index < raw_len {
        let candidate = candidates[candidate_index];
        let raw_value = raw_value(raw, raw_index);
        match candidate.cmp(&raw_value) {
            std::cmp::Ordering::Less => candidate_index += 1,
            std::cmp::Ordering::Greater => raw_index += 1,
            std::cmp::Ordering::Equal => {
                out.push(candidate);
                candidate_index += 1;
                raw_index += 1;
            }
        }
    }
    out
}

#[inline]
fn bitmap_contains(bitmap: &[u8], block_count: usize, value: u16) -> bool {
    let index = usize::from(value);
    index < block_count && bitmap[index / 8] & (1u8 << (index % 8)) != 0
}

fn intersect_raw_dense(raw: &[u8], raw_len: usize, bitmap: &[u8], block_count: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(raw_len);
    for index in 0..raw_len {
        let value = raw_value(raw, index);
        if bitmap_contains(bitmap, block_count, value) {
            out.push(value);
        }
    }
    out
}

fn intersect_dense_dense(
    left: &[u8],
    left_block_count: usize,
    right: &[u8],
    right_block_count: usize,
) -> Vec<u16> {
    let block_count = left_block_count
        .min(right_block_count)
        .min(u16::MAX as usize + 1);
    let bytes = block_count.div_ceil(8).min(left.len()).min(right.len());
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") && bytes >= 32 {
        // SAFETY: runtime detection guarantees AVX2 and the helper bounds every 32-byte load.
        return unsafe { intersect_dense_dense_avx2(left, right, bytes, block_count) };
    }
    intersect_dense_dense_scalar(left, right, bytes, block_count)
}

fn append_bitmap_byte(out: &mut Vec<u16>, byte: u8, base: usize, block_count: usize) {
    let mut bits = byte;
    while bits != 0 {
        let bit = bits.trailing_zeros() as usize;
        let value = base + bit;
        if value < block_count {
            out.push(value as u16);
        }
        bits &= bits - 1;
    }
}

fn intersect_dense_dense_scalar(
    left: &[u8],
    right: &[u8],
    bytes: usize,
    block_count: usize,
) -> Vec<u16> {
    let mut out = Vec::new();
    for index in 0..bytes {
        append_bitmap_byte(&mut out, left[index] & right[index], index * 8, block_count);
    }
    out
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn intersect_dense_dense_avx2(
    left: &[u8],
    right: &[u8],
    bytes: usize,
    block_count: usize,
) -> Vec<u16> {
    use std::arch::x86_64::{
        __m256i, _mm256_and_si256, _mm256_loadu_si256, _mm256_storeu_si256, _mm256_testz_si256,
    };

    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut scratch = [0u64; 4];
    while offset + 32 <= bytes {
        // SAFETY: loop bounds prove both unaligned 32-byte loads fit.
        let left_vector =
            unsafe { _mm256_loadu_si256(left.as_ptr().add(offset).cast::<__m256i>()) };
        let right_vector =
            unsafe { _mm256_loadu_si256(right.as_ptr().add(offset).cast::<__m256i>()) };
        let intersection = _mm256_and_si256(left_vector, right_vector);
        if _mm256_testz_si256(intersection, intersection) != 0 {
            offset += 32;
            continue;
        }
        // SAFETY: `scratch` is exactly 32 bytes and accepts one unaligned AVX2 store.
        unsafe { _mm256_storeu_si256(scratch.as_mut_ptr().cast::<__m256i>(), intersection) };
        for (word_index, mut bits) in scratch.iter().copied().enumerate() {
            let base = offset * 8 + word_index * 64;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                let value = base + bit;
                if value < block_count {
                    out.push(value as u16);
                }
                bits &= bits - 1;
            }
        }
        offset += 32;
    }
    for index in offset..bytes {
        append_bitmap_byte(&mut out, left[index] & right[index], index * 8, block_count);
    }
    out
}

#[cfg(test)]
mod simd_intersection_tests {
    use super::{
        intersect_dense_dense, intersect_direct_q3_postings, intersect_raw_raw,
        intersect_raw_raw_scalar,
    };
    use crate::vnext_q3::VNextQ3Posting;

    fn raw_bytes(values: &[u16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn dense_bytes(values: &[u16], block_count: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; block_count.div_ceil(8)];
        for &value in values {
            let index = usize::from(value);
            bytes[index / 8] |= 1u8 << (index % 8);
        }
        bytes
    }

    fn naive(left: &[u16], right: &[u16]) -> Vec<u16> {
        left.iter()
            .copied()
            .filter(|value| right.binary_search(value).is_ok())
            .collect()
    }

    #[test]
    fn raw_raw_simd_dispatch_matches_scalar_merge() {
        let left = (0u16..4096)
            .filter(|value| value % 3 == 0 || value % 17 == 0)
            .collect::<Vec<_>>();
        let right = (0u16..4096)
            .filter(|value| value % 5 == 0 || value % 19 == 0)
            .collect::<Vec<_>>();
        let left_bytes = raw_bytes(&left);
        let right_bytes = raw_bytes(&right);
        assert_eq!(
            intersect_raw_raw(&left_bytes, left.len(), &right_bytes, right.len()),
            intersect_raw_raw_scalar(&left_bytes, left.len(), &right_bytes, right.len())
        );
        assert_eq!(
            intersect_raw_raw(&left_bytes, left.len(), &right_bytes, right.len()),
            naive(&left, &right)
        );
    }

    #[test]
    fn raw_raw_simd_dispatch_handles_full_u16_domain() {
        let left = (32_000u32..=u16::MAX as u32)
            .filter(|value| value % 17 == 0 || value % 257 == 0)
            .map(|value| value as u16)
            .collect::<Vec<_>>();
        let right = (32_000u32..=u16::MAX as u32)
            .filter(|value| value % 19 == 0 || value % 263 == 0)
            .map(|value| value as u16)
            .collect::<Vec<_>>();
        let left_bytes = raw_bytes(&left);
        let right_bytes = raw_bytes(&right);
        assert_eq!(
            intersect_raw_raw(&left_bytes, left.len(), &right_bytes, right.len()),
            naive(&left, &right)
        );
    }

    #[test]
    fn dense_dense_simd_dispatch_matches_naive_membership() {
        let left = (0u16..4096)
            .filter(|value| value % 2 == 0 || value % 23 == 0)
            .collect::<Vec<_>>();
        let right = (0u16..4096)
            .filter(|value| value % 7 == 0 || value % 29 == 0)
            .collect::<Vec<_>>();
        let left_bytes = dense_bytes(&left, 4096);
        let right_bytes = dense_bytes(&right, 4096);
        assert_eq!(
            intersect_dense_dense(&left_bytes, 4096, &right_bytes, 4096),
            naive(&left, &right)
        );
    }

    #[test]
    fn dense_dense_simd_dispatch_handles_last_u16_bit() {
        let block_count = u16::MAX as usize + 1;
        let left = vec![0u16, 32_768, 65_000, u16::MAX];
        let right = vec![1u16, 32_768, 65_001, u16::MAX];
        let left_bytes = dense_bytes(&left, block_count);
        let right_bytes = dense_bytes(&right, block_count);
        assert_eq!(
            intersect_dense_dense(&left_bytes, block_count, &right_bytes, block_count),
            vec![32_768, u16::MAX]
        );
    }

    #[test]
    fn direct_posting_intersection_handles_raw_dense_and_second_anchor() {
        let primary_values = (0u16..2048)
            .filter(|value| value % 3 == 0)
            .collect::<Vec<_>>();
        let first_values = (0u16..2048)
            .filter(|value| value % 5 == 0)
            .collect::<Vec<_>>();
        let second_values = (0u16..2048)
            .filter(|value| value % 7 == 0)
            .collect::<Vec<_>>();
        let primary_bytes = raw_bytes(&primary_values);
        let first_bitmap = dense_bytes(&first_values, 2048);
        let second_bytes = raw_bytes(&second_values);
        let primary = VNextQ3Posting::raw_u16(&primary_bytes, primary_values.len());
        let first = VNextQ3Posting::dense_bitmap(&first_bitmap, first_values.len(), 2048);
        let second = VNextQ3Posting::raw_u16(&second_bytes, second_values.len());
        let expected = primary_values
            .iter()
            .copied()
            .filter(|value| first_values.binary_search(value).is_ok())
            .filter(|value| second_values.binary_search(value).is_ok())
            .collect::<Vec<_>>();
        assert_eq!(
            intersect_direct_q3_postings(primary, &[first, second]),
            expected
        );
    }
}

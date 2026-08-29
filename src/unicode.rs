use crate::unicode_tables::{
    CASEFOLD_DATA, CASEFOLD_INDEX, CCC, COMPOSE, DECOMP_DATA, DECOMP_INDEX,
};

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedText {
    bytes: Vec<u8>,
    byte_origins: Option<Vec<u32>>,
    original_len: u32,
}

impl NormalizedText {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn original_end(&self) -> u32 {
        self.original_len
    }

    pub fn origin_at(&self, normalized_byte: usize) -> u32 {
        self.byte_origins
            .as_ref()
            .and_then(|origins| origins.get(normalized_byte).copied())
            .unwrap_or(normalized_byte as u32)
    }

    pub fn scalar_view(&self) -> NormalizedScalars {
        let text = std::str::from_utf8(&self.bytes).expect("normalized text is UTF-8");
        let mut chars = Vec::with_capacity(text.chars().count());
        let mut origins = Vec::with_capacity(chars.capacity());
        for (byte_offset, ch) in text.char_indices() {
            chars.push(ch);
            origins.push(self.origin_at(byte_offset));
        }
        NormalizedScalars { chars, origins }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedScalars {
    pub chars: Vec<char>,
    pub origins: Vec<u32>,
}

#[derive(Clone, Copy, Debug)]
struct ScalarOrigin {
    cp: u32,
    origin: u32,
}

pub fn normalize_utf8(bytes: &[u8], case_sensitive: bool) -> Option<NormalizedText> {
    let text = std::str::from_utf8(bytes).ok()?;
    if byte_preserving_fast_path(text, case_sensitive) {
        let mut out = bytes.to_vec();
        if !case_sensitive {
            out.make_ascii_lowercase();
        }
        return Some(NormalizedText {
            bytes: out,
            byte_origins: None,
            original_len: bytes.len() as u32,
        });
    }

    let mut scalars = Vec::new();
    for (origin, ch) in text.char_indices() {
        canonical_decompose(ch as u32, origin as u32, &mut scalars);
    }
    canonical_reorder(&mut scalars);
    let mut scalars = canonical_compose(&scalars);

    if !case_sensitive {
        let mut folded = Vec::new();
        for scalar in scalars {
            if let Some(mapping) = casefold_mapping(scalar.cp) {
                folded.extend(mapping.iter().copied().map(|cp| ScalarOrigin {
                    cp,
                    origin: scalar.origin,
                }));
            } else {
                folded.push(scalar);
            }
        }
        let mut decomposed = Vec::new();
        for scalar in folded {
            canonical_decompose(scalar.cp, scalar.origin, &mut decomposed);
        }
        canonical_reorder(&mut decomposed);
        scalars = canonical_compose(&decomposed);
    }

    let mut out = Vec::new();
    let mut origins = Vec::new();
    for scalar in scalars {
        let ch = char::from_u32(scalar.cp).expect("Unicode table contains scalar values");
        let mut buf = [0_u8; 4];
        let encoded = ch.encode_utf8(&mut buf).as_bytes();
        out.extend_from_slice(encoded);
        origins.extend(std::iter::repeat_n(scalar.origin, encoded.len()));
    }
    let identity = origins
        .iter()
        .enumerate()
        .all(|(position, origin)| *origin as usize == position);
    Some(NormalizedText {
        bytes: out,
        byte_origins: (!identity).then_some(origins),
        original_len: bytes.len() as u32,
    })
}

pub fn normalize_str(text: &str, case_sensitive: bool) -> NormalizedText {
    normalize_utf8(text.as_bytes(), case_sensitive).expect("str is valid UTF-8")
}

pub fn fold_for_index(bytes: &[u8]) -> Vec<u8> {
    normalize_utf8(bytes, false)
        .map(NormalizedText::into_bytes)
        .unwrap_or_else(|| bytes.iter().map(u8::to_ascii_lowercase).collect())
}

pub fn is_byte_preserving_fast_path(text: &str, case_sensitive: bool) -> bool {
    byte_preserving_fast_path(text, case_sensitive)
}

fn byte_preserving_fast_path(text: &str, case_sensitive: bool) -> bool {
    if text.is_ascii() {
        return true;
    }
    let mut previous_non_ascii = None::<u32>;
    for ch in text.chars() {
        if ch.is_ascii() {
            previous_non_ascii = None;
            continue;
        }
        let cp = ch as u32;
        if combining_class(cp) != 0
            || decomposition_mapping(cp).is_some()
            || (!case_sensitive && casefold_mapping(cp).is_some())
        {
            return false;
        }
        if let Some(left) = previous_non_ascii
            && compose_pair(left, cp).is_some()
        {
            return false;
        }
        previous_non_ascii = Some(cp);
    }
    true
}

fn combining_class(cp: u32) -> u8 {
    CCC.binary_search_by_key(&cp, |(value, _)| *value)
        .ok()
        .map(|index| CCC[index].1)
        .unwrap_or(0)
}

fn decomposition_mapping(cp: u32) -> Option<&'static [u32]> {
    DECOMP_INDEX
        .binary_search_by_key(&cp, |(value, _, _)| *value)
        .ok()
        .map(|index| {
            let (_, offset, len) = DECOMP_INDEX[index];
            &DECOMP_DATA[offset as usize..offset as usize + len as usize]
        })
}

fn casefold_mapping(cp: u32) -> Option<&'static [u32]> {
    CASEFOLD_INDEX
        .binary_search_by_key(&cp, |(value, _, _)| *value)
        .ok()
        .map(|index| {
            let (_, offset, len) = CASEFOLD_INDEX[index];
            &CASEFOLD_DATA[offset as usize..offset as usize + len as usize]
        })
}

fn canonical_decompose(cp: u32, origin: u32, out: &mut Vec<ScalarOrigin>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let s_index = cp - S_BASE;
        let l = L_BASE + s_index / N_COUNT;
        let v = V_BASE + (s_index % N_COUNT) / T_COUNT;
        let t = T_BASE + s_index % T_COUNT;
        out.push(ScalarOrigin { cp: l, origin });
        out.push(ScalarOrigin { cp: v, origin });
        if t != T_BASE {
            out.push(ScalarOrigin { cp: t, origin });
        }
        return;
    }
    if let Some(mapping) = decomposition_mapping(cp) {
        for mapped in mapping {
            canonical_decompose(*mapped, origin, out);
        }
    } else {
        out.push(ScalarOrigin { cp, origin });
    }
}

fn canonical_reorder(scalars: &mut [ScalarOrigin]) {
    for index in 1..scalars.len() {
        let ccc = combining_class(scalars[index].cp);
        if ccc == 0 {
            continue;
        }
        let mut position = index;
        while position > 0 {
            let previous = combining_class(scalars[position - 1].cp);
            if previous == 0 || previous <= ccc {
                break;
            }
            scalars.swap(position - 1, position);
            position -= 1;
        }
    }
}

fn canonical_compose(input: &[ScalarOrigin]) -> Vec<ScalarOrigin> {
    let mut out = Vec::<ScalarOrigin>::with_capacity(input.len());
    let mut starter_index = None::<usize>;
    let mut last_ccc = 0_u8;

    for scalar in input.iter().copied() {
        let ccc = combining_class(scalar.cp);
        if let Some(index) = starter_index
            && (last_ccc < ccc || last_ccc == 0)
            && let Some(composed) = compose_pair(out[index].cp, scalar.cp)
        {
            out[index].cp = composed;
            continue;
        }
        out.push(scalar);
        if ccc == 0 {
            starter_index = Some(out.len() - 1);
        }
        last_ccc = ccc;
    }
    out
}

fn compose_pair(a: u32, b: u32) -> Option<u32> {
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        let l_index = a - L_BASE;
        let v_index = b - V_BASE;
        return Some(S_BASE + (l_index * V_COUNT + v_index) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return Some(a + (b - T_BASE));
    }
    COMPOSE
        .binary_search_by(|(left, right, _)| (*left, *right).cmp(&(a, b)))
        .ok()
        .map(|index| COMPOSE[index].2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unicode_tables::UNICODE_VERSION;

    #[test]
    fn unicode_version_is_frozen() {
        assert_eq!(UNICODE_VERSION, "15.1.0");
    }

    #[test]
    fn nfc_and_casefold_semantics_match_frozen_examples() {
        let pre = normalize_str("é", true);
        let decomposed = normalize_str("e\u{301}", true);
        assert_eq!(pre.bytes(), decomposed.bytes());
        assert_eq!(decomposed.origin_at(0), 0);

        assert_eq!(normalize_str("Straße", false).bytes(), b"strasse");
        assert_eq!(normalize_str("Σςσ", false).bytes(), "σσσ".as_bytes());
        assert_ne!(normalize_str("Ａ", false).bytes(), b"a");
    }

    #[test]
    fn hangul_composition_is_algorithmic() {
        let composed = normalize_str("\u{1100}\u{1161}", true);
        assert_eq!(composed.bytes(), "가".as_bytes());
    }

    #[test]
    fn generated_python_unicode_15_1_oracle_vectors_match() {
        fn decode_hex(value: &str) -> Vec<u8> {
            value
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    let text = std::str::from_utf8(pair).unwrap();
                    u8::from_str_radix(text, 16).unwrap()
                })
                .collect()
        }
        for (line_number, line) in include_str!("../tests/unicode_oracle_vectors.txt")
            .lines()
            .enumerate()
        {
            let mut fields = line.split('|');
            let input = decode_hex(fields.next().unwrap());
            let sensitive = decode_hex(fields.next().unwrap());
            let folded = decode_hex(fields.next().unwrap());
            assert_eq!(
                normalize_utf8(&input, true).unwrap().bytes(),
                sensitive,
                "sensitive vector line {}",
                line_number + 1
            );
            assert_eq!(
                normalize_utf8(&input, false).unwrap().bytes(),
                folded,
                "folded vector line {}",
                line_number + 1
            );
        }
    }

    #[test]
    fn every_table_composition_pair_normalizes_to_its_composed_scalar() {
        for &(left, right, composed) in COMPOSE {
            let input = format!(
                "{}{}",
                char::from_u32(left).unwrap(),
                char::from_u32(right).unwrap()
            );
            let expected = char::from_u32(composed).unwrap().to_string();
            assert_eq!(normalize_str(&input, true).bytes(), expected.as_bytes());
        }
    }

    #[test]
    fn japanese_ascii_is_byte_preserving_but_unicode_special_cases_are_not() {
        assert!(is_byte_preserving_fast_path("日本語 ABC", false));
        assert!(!is_byte_preserving_fast_path("Straße", false));
        assert!(!is_byte_preserving_fast_path("e\u{301}", true));
    }
}

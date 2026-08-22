from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_head = '''        let mask_base = unit_index * 32;
        q3_scratch.clear();
        q3_touched_words.clear();
        q2_touched_words.clear();
        let mut previous2 = 0u8;
        let mut previous1 = 0u8;
        for (index, &byte) in content.iter().enumerate() {
            content_q1mask[mask_base + usize::from(byte / 8)] |= 1u8 << (byte % 8);
'''
new_head = '''        let mask_base = unit_index * 32;
        q3_scratch.clear();
        q3_touched_words.clear();
        q2_touched_words.clear();
        let mut q1_seen = [0u64; 4];
        let mut previous2 = 0u8;
        let mut previous1 = 0u8;
        for (index, &byte) in content.iter().enumerate() {
            let q1_word = usize::from(byte >> 6);
            q1_seen[q1_word] |= 1u64 << (byte & 63);
'''
if text.count(old_head) != 1:
    raise SystemExit(f"q1 loop head: expected 1 match, got {text.count(old_head)}")
text = text.replace(old_head, new_head, 1)

old_tail = '''            previous2 = previous1;
            previous1 = byte;
        }
        for &key in &q3_scratch {
'''
new_tail = '''            previous2 = previous1;
            previous1 = byte;
        }
        for (word_index, word) in q1_seen.into_iter().enumerate() {
            let begin = mask_base + word_index * 8;
            content_q1mask[begin..begin + 8].copy_from_slice(&word.to_le_bytes());
        }
        for &key in &q3_scratch {
'''
if text.count(old_tail) != 1:
    raise SystemExit(f"q1 loop tail: expected 1 match, got {text.count(old_tail)}")
text = text.replace(old_tail, new_tail, 1)

path.write_text(text, encoding="utf-8")
print("local q1 mask transform applied")

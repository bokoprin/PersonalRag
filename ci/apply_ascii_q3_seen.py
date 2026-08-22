from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_decl = '''    let mut q3_scratch = Vec::<u32>::new();
    // 2^24 q3 keys -> 2 MiB bitset. Reuse it for every ContentUnit and clear only touched
    // words, eliminating the per-unit O(n log n) q3 sort/dedup.
    let mut q3_seen = vec![0u64; 1usize << 18];
    let mut q3_touched_words = Vec::<usize>::new();
'''
new_decl = '''    let mut q3_scratch = Vec::<u32>::new();
    // ASCII q3 keys occupy only 21 bits (7 bits per byte), so keep their hot dedup state in a
    // 256 KiB bitset. Non-ASCII trigrams retain the full 24-bit map, allocated lazily.
    let mut ascii_q3_seen = vec![0u64; 1usize << 15];
    let mut ascii_q3_touched_words = Vec::<usize>::new();
    let mut wide_q3_seen: Option<Vec<u64>> = None;
    let mut wide_q3_touched_words = Vec::<usize>::new();
'''
if text.count(old_decl) != 1:
    raise SystemExit(f"q3 declaration block: expected 1 match, got {text.count(old_decl)}")
text = text.replace(old_decl, new_decl, 1)

old_clear = '''        q3_scratch.clear();
        q3_touched_words.clear();
        q2_touched_words.clear();
'''
new_clear = '''        q3_scratch.clear();
        ascii_q3_touched_words.clear();
        wide_q3_touched_words.clear();
        q2_touched_words.clear();
'''
if text.count(old_clear) != 1:
    raise SystemExit(f"q3 clear block: expected 1 match, got {text.count(old_clear)}")
text = text.replace(old_clear, new_clear, 1)

old_q3 = '''            if index >= 2 {
                let key = k3(previous2, previous1, byte);
                let word_index = (key >> 6) as usize;
                let bit = 1u64 << (key & 63);
                let word = q3_seen[word_index];
                if word & bit == 0 {
                    if word == 0 {
                        q3_touched_words.push(word_index);
                    }
                    q3_seen[word_index] = word | bit;
                    q3_scratch.push(key);
                }
            }
'''
new_q3 = '''            if index >= 2 {
                let key = k3(previous2, previous1, byte);
                let is_new = if previous2 | previous1 | byte < 0x80 {
                    let compact = (usize::from(previous2) << 14)
                        | (usize::from(previous1) << 7)
                        | usize::from(byte);
                    let word_index = compact >> 6;
                    let bit = 1u64 << (compact & 63);
                    let word = ascii_q3_seen[word_index];
                    if word & bit == 0 {
                        if word == 0 {
                            ascii_q3_touched_words.push(word_index);
                        }
                        ascii_q3_seen[word_index] = word | bit;
                        true
                    } else {
                        false
                    }
                } else {
                    let seen = wide_q3_seen.get_or_insert_with(|| vec![0u64; 1usize << 18]);
                    let word_index = (key >> 6) as usize;
                    let bit = 1u64 << (key & 63);
                    let word = seen[word_index];
                    if word & bit == 0 {
                        if word == 0 {
                            wide_q3_touched_words.push(word_index);
                        }
                        seen[word_index] = word | bit;
                        true
                    } else {
                        false
                    }
                };
                if is_new {
                    q3_scratch.push(key);
                }
            }
'''
if text.count(old_q3) != 1:
    raise SystemExit(f"q3 dedup block: expected 1 match, got {text.count(old_q3)}")
text = text.replace(old_q3, new_q3, 1)

old_reset = '''        for &word_index in &q3_touched_words {
            q3_seen[word_index] = 0;
        }
'''
new_reset = '''        for &word_index in &ascii_q3_touched_words {
            ascii_q3_seen[word_index] = 0;
        }
        if let Some(seen) = wide_q3_seen.as_mut() {
            for &word_index in &wide_q3_touched_words {
                seen[word_index] = 0;
            }
        }
'''
if text.count(old_reset) != 1:
    raise SystemExit(f"q3 reset block: expected 1 match, got {text.count(old_reset)}")
text = text.replace(old_reset, new_reset, 1)

path.write_text(text, encoding="utf-8")
print("ascii q3 compact seen transform applied")

from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_decl = "    let mut q3_scratch = Vec::<u32>::new();\n"
if text.count(old_decl) != 1:
    raise SystemExit(f"q3 scratch declaration: expected 1 match, got {text.count(old_decl)}")
text = text.replace(old_decl, "", 1)

old_clear = "        q3_scratch.clear();\n"
if text.count(old_clear) != 1:
    raise SystemExit(f"q3 scratch clear: expected 1 match, got {text.count(old_clear)}")
text = text.replace(old_clear, "", 1)

old_emit = '''            if index >= 2 {
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
            previous2 = previous1;
            previous1 = byte;
        }
        for &key in &q3_scratch {
            if use_packed_shards {
                let high = (key >> 16) as usize;
                let packed = ((key & 0xffff) << 16) | unit_id;
                content_q3_shards[high].push(packed);
            } else {
                content_q3_pairs.push((u64::from(key) << 32) | u64::from(unit_id));
            }
        }
'''
new_emit = '''            if index >= 2 {
                let key = k3(previous2, previous1, byte);
                let word_index = (key >> 6) as usize;
                let bit = 1u64 << (key & 63);
                let word = q3_seen[word_index];
                if word & bit == 0 {
                    if word == 0 {
                        q3_touched_words.push(word_index);
                    }
                    q3_seen[word_index] = word | bit;
                    if use_packed_shards {
                        let high = (key >> 16) as usize;
                        let packed = ((key & 0xffff) << 16) | unit_id;
                        content_q3_shards[high].push(packed);
                    } else {
                        content_q3_pairs.push((u64::from(key) << 32) | u64::from(unit_id));
                    }
                }
            }
            previous2 = previous1;
            previous1 = byte;
        }
'''
if text.count(old_emit) != 1:
    raise SystemExit(f"q3 emission block: expected 1 match, got {text.count(old_emit)}")
text = text.replace(old_emit, new_emit, 1)

path.write_text(text, encoding="utf-8")
print("direct q3 emission transform applied")

from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_packed_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    for (high, shard) in shards.iter_mut().enumerate() {
'''
new_packed_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    let mut ids = Vec::<u32>::new();
    for (high, shard) in shards.iter_mut().enumerate() {
'''
if text.count(old_packed_head) != 1:
    raise SystemExit(f"packed head: expected 1 match, got {text.count(old_packed_head)}")
text = text.replace(old_packed_head, new_packed_head, 1)

old_packed_ids = '''            let ids = shard[begin..position]
                .iter()
                .map(|packed| packed & 0xffff)
                .collect::<Vec<_>>();
            let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
'''
new_packed_ids = '''            ids.clear();
            ids.extend(shard[begin..position].iter().map(|packed| packed & 0xffff));
            let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
'''
if text.count(old_packed_ids) != 1:
    raise SystemExit(f"packed ids: expected 1 match, got {text.count(old_packed_ids)}")
text = text.replace(old_packed_ids, new_packed_ids, 1)

old_pairs_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    let mut position = 0usize;
'''
new_pairs_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    let mut ids = Vec::<u32>::new();
    let mut position = 0usize;
'''
if text.count(old_pairs_head) != 1:
    raise SystemExit(f"pairs head: expected 1 match, got {text.count(old_pairs_head)}")
text = text.replace(old_pairs_head, new_pairs_head, 1)

old_pairs_ids = '''        let ids = q3_pairs[begin..position]
            .iter()
            .map(|pair| *pair as u32)
            .collect::<Vec<_>>();
        let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
'''
new_pairs_ids = '''        ids.clear();
        ids.extend(q3_pairs[begin..position].iter().map(|pair| *pair as u32));
        let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
'''
if text.count(old_pairs_ids) != 1:
    raise SystemExit(f"pairs ids: expected 1 match, got {text.count(old_pairs_ids)}")
text = text.replace(old_pairs_ids, new_pairs_ids, 1)

path.write_text(text, encoding="utf-8")
print("q3 posting ids scratch transform applied")

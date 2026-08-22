from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

old_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    for (high, shard) in shards.iter_mut().enumerate() {
'''
new_head = '''    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    let mut ids_scratch = Vec::<u32>::new();
    for (high, shard) in shards.iter_mut().enumerate() {
'''
if text.count(old_head) != 1:
    raise SystemExit(f"packed shard head: expected 1 match, got {text.count(old_head)}")
text = text.replace(old_head, new_head, 1)

old_block = '''            let ids = shard[begin..position]
                .iter()
                .map(|packed| packed & 0xffff)
                .collect::<Vec<_>>();
            let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
            let count = u32::try_from(ids.len())
                .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
new_block = '''            ids_scratch.clear();
            ids_scratch.extend(shard[begin..position].iter().map(|packed| packed & 0xffff));
            let (encoding, offset, bytes) = encode_q3(&ids_scratch, universe, &mut q3blob)?;
            let count = u32::try_from(ids_scratch.len())
                .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
'''
if text.count(old_block) != 1:
    raise SystemExit(f"packed shard ids block: expected 1 match, got {text.count(old_block)}")
text = text.replace(old_block, new_block, 1)

path.write_text(text, encoding="utf-8")
print("q3 scratch reuse transform applied")

from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")
old = '''    let mut unit_text_off = Vec::with_capacity(unit_sources.len() + 1);
    let mut texts = Vec::new();
    let mut unit_doc_off = Vec::with_capacity(unit_sources.len() + 1);
    let mut unit_docs_flat = Vec::new();
'''
new = '''    let mut unit_text_off = Vec::with_capacity(unit_sources.len() + 1);
    let total_text_bytes = unit_sources.iter().try_fold(0usize, |total, &source| {
        total
            .checked_add(docs[source].normalized_content.len())
            .ok_or_else(|| SearchError::Format("text blob size overflow".into()))
    })?;
    let mut texts = Vec::with_capacity(total_text_bytes);
    let mut unit_doc_off = Vec::with_capacity(unit_sources.len() + 1);
    let mut unit_docs_flat = Vec::with_capacity(doc_count);
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"text allocation block: expected 1 match, got {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("segment text preallocation transform applied")

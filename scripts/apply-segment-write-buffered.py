from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

replacements = [
    (
        "use std::io::{Read, Write};",
        "use std::io::{BufWriter, Read, Write};",
    ),
    (
        "const MIB: u64 = 1024 * 1024;",
        "const MIB: u64 = 1024 * 1024;\nconst SEGMENT_WRITE_BUFFER_BYTES: usize = 1024 * 1024;",
    ),
    (
        """    let mut file = OpenOptions::new()\n        .create(true)\n        .truncate(true)\n        .write(true)\n        .open(path)?;\n    let open = open_started.elapsed();\n""",
        """    let file = OpenOptions::new()\n        .create(true)\n        .truncate(true)\n        .write(true)\n        .open(path)?;\n    let open = open_started.elapsed();\n    let mut file = BufWriter::with_capacity(SEGMENT_WRITE_BUFFER_BYTES, file);\n""",
    ),
    (
        """    file.write_all(FOOTER_MAGIC)?;\n    file.write_all(&hash.to_le_bytes())?;\n    let body = body_started.elapsed();\n""",
        """    file.write_all(FOOTER_MAGIC)?;\n    file.write_all(&hash.to_le_bytes())?;\n    file.flush()?;\n    let body = body_started.elapsed();\n""",
    ),
    (
        "if file.metadata()?.len() != final_size {",
        "if file.get_ref().metadata()?.len() != final_size {",
    ),
    (
        """    let sync = if durable {\n        let sync_started = Instant::now();\n        file.sync_all()?;\n        sync_started.elapsed()\n    } else {\n""",
        """    let sync = if durable {\n        let sync_started = Instant::now();\n        file.get_ref().sync_all()?;\n        sync_started.elapsed()\n    } else {\n""",
    ),
    (
        "fn write_hashed(file: &mut File, hash: &mut u64, bytes: &[u8]) -> Result<()> {",
        "fn write_hashed<W: Write>(file: &mut W, hash: &mut u64, bytes: &[u8]) -> Result<()> {",
    ),
    (
        "fn write_padding(file: &mut File, hash: &mut u64, count: usize) -> Result<()> {",
        "fn write_padding<W: Write>(file: &mut W, hash: &mut u64, count: usize) -> Result<()> {",
    ),
    (
        """fn stream_u32_values(\n    file: &mut File,\n""",
        """fn stream_u32_values<W: Write>(\n    file: &mut W,\n""",
    ),
    (
        """fn stream_u64_values(\n    file: &mut File,\n""",
        """fn stream_u64_values<W: Write>(\n    file: &mut W,\n""",
    ),
]

for old, new in replacements:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one match, found {count}: {old[:100]!r}")
    text = text.replace(old, new, 1)

path.write_text(text, encoding="utf-8", newline="\n")
print("SEGMENT_WRITE_BUFFER_PATCH_APPLIED")

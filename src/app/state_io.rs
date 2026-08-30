use super::{AppError, Result};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        return Ok(());
    }
    let temp = path.with_extension("tmp");
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

pub(super) fn numbered_files(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> Result<Vec<(u64, PathBuf)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(middle) = name
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            && let Ok(generation) = middle.parse::<u64>()
        {
            out.push((generation, entry.path()));
        }
    }
    Ok(out)
}

pub(super) fn next_number(dir: &Path, prefix: &str, suffix: &str) -> Result<u64> {
    Ok(numbered_files(dir, prefix, suffix)?
        .into_iter()
        .map(|(generation, _)| generation)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1))
}

pub(super) fn parse_key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

pub(super) fn parse_u64(values: &HashMap<String, String>, key: &str) -> Option<u64> {
    values.get(key)?.parse().ok()
}

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(super) fn hex_decode(value: &str) -> std::result::Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| ())?;
        out.push(u8::from_str_radix(text, 16).map_err(|_| ())?);
    }
    Ok(out)
}

pub(super) fn read_u32(bytes: &[u8], offset: usize, what: &'static str) -> Result<u32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| AppError::InvalidState(format!("truncated {what}")))?;
    Ok(u32::from_le_bytes(value.try_into().map_err(|_| {
        AppError::InvalidState(format!("invalid {what}"))
    })?))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize, what: &'static str) -> Result<u64> {
    let value = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| AppError::InvalidState(format!("truncated {what}")))?;
    Ok(u64::from_le_bytes(value.try_into().map_err(|_| {
        AppError::InvalidState(format!("invalid {what}"))
    })?))
}

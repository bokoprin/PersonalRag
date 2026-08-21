use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io;

pub(crate) const HEADER_SIZE: usize = 512;
pub(crate) const SECTION_COUNT: usize = 23;
pub(crate) const SEG_MAGIC: &[u8; 8] = b"PRSEG005";
pub(crate) const FOOTER_MAGIC: &[u8; 8] = b"PRFTR005";
pub(crate) const MANIFEST_MAGIC: &str = "PRMAN001";
pub(crate) const FNV_OFFSET: u64 = 1_469_598_103_934_665_603;
pub(crate) const FNV_PRIME: u64 = 1_099_511_628_211;

pub(crate) mod section {
    pub const DOC_NAME_OFF: usize = 0;
    pub const NAME_BLOB: usize = 1;
    pub const UNIT_TEXT_OFF: usize = 2;
    pub const TEXT_BLOB: usize = 3;
    pub const UNIT_DOC_OFF: usize = 4;
    pub const UNIT_DOCS: usize = 5;
    pub const DOC_UNIT: usize = 6;
    pub const CQ1OFF: usize = 7;
    pub const CQ1POST: usize = 8;
    pub const CQ1MASK: usize = 9;
    pub const CQ1RARE: usize = 10;
    pub const CQ2OFF: usize = 11;
    pub const CQ2POST: usize = 12;
    pub const CQ3DIR: usize = 13;
    pub const CQ3POST: usize = 14;
    pub const NQ1OFF: usize = 15;
    pub const NQ1POST: usize = 16;
    pub const NQ2OFF: usize = 17;
    pub const NQ2POST: usize = 18;
    pub const NQ3DIR: usize = 19;
    pub const NQ3POST: usize = 20;
    pub const TEXT_BLOCK_DIR: usize = 21;
    pub const TEXT_BLOCK_BLOB: usize = 22;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Desc {
    pub off: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum BuilderKind {
    Direct = 1,
    Dedup = 2,
    RunAware = 3,
}

impl BuilderKind {
    pub(crate) fn from_u32(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::Direct),
            2 => Ok(Self::Dedup),
            3 => Ok(Self::RunAware),
            _ => Err(SearchError::Format(format!("bad builder kind {value}"))),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Dedup => "dedup",
            Self::RunAware => "runaware",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Q3Encoding {
    InlineU32 = 1,
    DeltaVarint = 2,
    Block256Bitmap = 3,
    DenseBitset = 4,
}

impl Q3Encoding {
    pub(crate) fn from_packed(packed: u32) -> Result<Self> {
        match (packed >> 30) + 1 {
            1 => Ok(Self::InlineU32),
            2 => Ok(Self::DeltaVarint),
            3 => Ok(Self::Block256Bitmap),
            4 => Ok(Self::DenseBitset),
            value => Err(SearchError::Format(format!("bad q3 encoding {value}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Q3DirKind {
    Full16 = 1,
    Prefix10 = 2,
}

impl Q3DirKind {
    pub(crate) fn from_u32(value: u32) -> Result<Self> {
        match value {
            0 | 1 => Ok(Self::Full16),
            2 => Ok(Self::Prefix10),
            _ => Err(SearchError::Format(format!(
                "bad q3 directory kind {value}"
            ))),
        }
    }
}

#[derive(Debug)]
pub enum SearchError {
    Io(io::Error),
    Format(String),
    InvalidArgument(String),
}

impl Display for SearchError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, f),
            Self::Format(message) | Self::InvalidArgument(message) => f.write_str(message),
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(_) | Self::InvalidArgument(_) => None,
        }
    }
}

impl From<io::Error> for SearchError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, SearchError>;

pub(crate) const fn align8(value: u64) -> u64 {
    (value + 7) & !7
}

pub(crate) fn rd16(bytes: &[u8], off: usize) -> Result<u16> {
    let data = bytes
        .get(off..off + 2)
        .ok_or_else(|| SearchError::Format("u16 read out of bounds".into()))?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

pub(crate) fn rd32(bytes: &[u8], off: usize) -> Result<u32> {
    let data = bytes
        .get(off..off + 4)
        .ok_or_else(|| SearchError::Format("u32 read out of bounds".into()))?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

pub(crate) fn rd64(bytes: &[u8], off: usize) -> Result<u64> {
    let data = bytes
        .get(off..off + 8)
        .ok_or_else(|| SearchError::Format("u64 read out of bounds".into()))?;
    Ok(u64::from_le_bytes(
        data.try_into().expect("fixed slice length"),
    ))
}

pub(crate) fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn write_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn write_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

pub(crate) const fn k2(a: u8, b: u8) -> u16 {
    (a as u16) | ((b as u16) << 8)
}

pub(crate) const fn k3(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16)
}

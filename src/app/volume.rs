use super::state_io::{
    atomic_write_new, hex_decode, hex_encode, next_number, numbered_files, parse_key_values,
    parse_u64,
};
use super::{AppError, Result};
use crate::persistent::crc64_ecma;
use std::fs;
#[cfg(windows)]
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VolumeKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredVolume {
    pub key: VolumeKey,
    pub mount: PathBuf,
    pub serial: u32,
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub volumes: PathBuf,
    pub catalog: PathBuf,
}

impl AppPaths {
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            volumes: root.join("volumes"),
            catalog: root.join("catalog"),
            root,
        }
    }

    pub fn default_for_current_user() -> Result<Self> {
        #[cfg(windows)]
        {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            Ok(Self::for_root(base.join("PersonalRag")))
        }
        #[cfg(not(windows))]
        {
            if let Some(base) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
                return Ok(Self::for_root(base.join("PersonalRag")));
            }
            Ok(Self::for_root(std::env::temp_dir().join("PersonalRag")))
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.volumes)?;
        fs::create_dir_all(&self.catalog)?;
        Ok(())
    }

    pub fn volume_store(&self, key: &VolumeKey) -> PathBuf {
        self.volumes
            .join(format!("{:016x}", crc64_ecma(key.0.as_bytes())))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumePhase {
    Discovered,
    MetadataBuilding,
    MetadataReady,
    ContentBuilding,
    ContentCatchUp,
    Ready,
    Degraded,
}

impl VolumePhase {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::MetadataBuilding => "metadata-building",
            Self::MetadataReady => "metadata-ready",
            Self::ContentBuilding => "content-building",
            Self::ContentCatchUp => "content-catch-up",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "discovered" => Ok(Self::Discovered),
            "metadata-building" => Ok(Self::MetadataBuilding),
            "metadata-ready" => Ok(Self::MetadataReady),
            "content-building" => Ok(Self::ContentBuilding),
            "content-catch-up" => Ok(Self::ContentCatchUp),
            "ready" => Ok(Self::Ready),
            "degraded" => Ok(Self::Degraded),
            _ => Err(AppError::InvalidState(format!(
                "unknown volume phase: {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct VolumeManifest {
    pub generation: u64,
    pub key: VolumeKey,
    pub mount: PathBuf,
    pub phase: VolumePhase,
    pub metadata_generation: u64,
    pub metadata_file: Option<String>,
    pub metadata_records: usize,
    pub inaccessible_directories: usize,
}

impl VolumeManifest {
    pub(super) fn initial(volume: &DiscoveredVolume) -> Self {
        Self {
            generation: 0,
            key: volume.key.clone(),
            mount: volume.mount.clone(),
            phase: VolumePhase::Discovered,
            metadata_generation: 0,
            metadata_file: None,
            metadata_records: 0,
            inaccessible_directories: 0,
        }
    }
}

pub(crate) fn write_volume_manifest(store: &Path, manifest: &VolumeManifest) -> Result<PathBuf> {
    fs::create_dir_all(store)?;
    let path = store.join(format!("volume-{:020}.state", manifest.generation));
    let metadata_file = manifest.metadata_file.as_deref().unwrap_or("");
    let content = format!(
        "version=1\nkey={}\nmount={}\nphase={}\nmetadata_generation={}\nmetadata_file={}\nmetadata_records={}\ninaccessible={}\n",
        hex_encode(manifest.key.0.as_bytes()),
        hex_encode(manifest.mount.to_string_lossy().as_bytes()),
        manifest.phase.as_str(),
        manifest.metadata_generation,
        metadata_file,
        manifest.metadata_records,
        manifest.inaccessible_directories
    );
    atomic_write_new(&path, content.as_bytes())?;
    Ok(path)
}

pub fn load_volume_manifest(store: &Path) -> Result<Option<VolumeManifest>> {
    let mut states = numbered_files(store, "volume-", ".state")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in states {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let values = parse_key_values(&text);
        let Some(key_hex) = values.get("key") else {
            continue;
        };
        let Some(mount_hex) = values.get("mount") else {
            continue;
        };
        let Ok(key_bytes) = hex_decode(key_hex) else {
            continue;
        };
        let Ok(mount_bytes) = hex_decode(mount_hex) else {
            continue;
        };
        let Ok(key) = String::from_utf8(key_bytes) else {
            continue;
        };
        let Ok(mount) = String::from_utf8(mount_bytes) else {
            continue;
        };
        let Some(phase_text) = values.get("phase") else {
            continue;
        };
        let Ok(phase) = VolumePhase::parse(phase_text) else {
            continue;
        };
        let Some(metadata_generation) = parse_u64(&values, "metadata_generation") else {
            continue;
        };
        let Some(metadata_records) = parse_u64(&values, "metadata_records") else {
            continue;
        };
        let Some(inaccessible) = parse_u64(&values, "inaccessible") else {
            continue;
        };
        let metadata_file = values
            .get("metadata_file")
            .filter(|value| !value.is_empty())
            .cloned();
        if let Some(file_name) = metadata_file.as_deref()
            && !store.join("metadata").join(file_name).exists()
        {
            continue;
        }
        return Ok(Some(VolumeManifest {
            generation,
            key: VolumeKey(key),
            mount: PathBuf::from(mount),
            phase,
            metadata_generation,
            metadata_file,
            metadata_records: metadata_records as usize,
            inaccessible_directories: inaccessible as usize,
        }));
    }
    Ok(None)
}

pub(crate) fn write_app_catalog(paths: &AppPaths, volumes: &[DiscoveredVolume]) -> Result<()> {
    fs::create_dir_all(&paths.catalog)?;
    let generation = next_number(&paths.catalog, "catalog-", ".state")?;
    let mut content = String::from("version=1\n");
    for volume in volumes {
        content.push_str("volume=");
        content.push_str(&hex_encode(volume.key.0.as_bytes()));
        content.push(',');
        content.push_str(&hex_encode(volume.mount.to_string_lossy().as_bytes()));
        content.push(',');
        content.push_str(&volume.serial.to_string());
        content.push('\n');
    }
    atomic_write_new(
        &paths
            .catalog
            .join(format!("catalog-{generation:020}.state")),
        content.as_bytes(),
    )?;
    Ok(())
}

#[cfg(windows)]
pub fn discover_fixed_volumes() -> Result<Vec<DiscoveredVolume>> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;

    type Bool = i32;
    const DRIVE_FIXED: u32 = 3;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDriveStringsW(buffer_length: u32, buffer: *mut u16) -> u32;
        fn GetDriveTypeW(root_path_name: *const u16) -> u32;
        fn GetVolumeNameForVolumeMountPointW(
            volume_mount_point: *const u16,
            volume_name: *mut u16,
            buffer_length: u32,
        ) -> Bool;
        fn GetVolumeInformationW(
            root_path_name: *const u16,
            volume_name_buffer: *mut u16,
            volume_name_size: u32,
            volume_serial_number: *mut u32,
            maximum_component_length: *mut u32,
            file_system_flags: *mut u32,
            file_system_name_buffer: *mut u16,
            file_system_name_size: u32,
        ) -> Bool;
    }

    let needed = unsafe { GetLogicalDriveStringsW(0, null_mut()) };
    if needed == 0 {
        return Err(AppError::Io(io::Error::last_os_error()));
    }
    let mut buffer = vec![0_u16; needed as usize + 1];
    let written = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if written == 0 {
        return Err(AppError::Io(io::Error::last_os_error()));
    }

    let mut volumes = Vec::new();
    let mut start = 0_usize;
    while start < written as usize {
        let Some(relative_end) = buffer[start..].iter().position(|value| *value == 0) else {
            break;
        };
        if relative_end == 0 {
            break;
        }
        let end = start + relative_end;
        let root_wide = buffer[start..=end].to_vec();
        if unsafe { GetDriveTypeW(root_wide.as_ptr()) } == DRIVE_FIXED {
            let mount = PathBuf::from(OsString::from_wide(&buffer[start..end]));
            let mut serial = 0_u32;
            let _ = unsafe {
                GetVolumeInformationW(
                    root_wide.as_ptr(),
                    null_mut(),
                    0,
                    &mut serial,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    0,
                )
            };
            let mut guid_buffer = vec![0_u16; 128];
            let has_guid = unsafe {
                GetVolumeNameForVolumeMountPointW(
                    root_wide.as_ptr(),
                    guid_buffer.as_mut_ptr(),
                    guid_buffer.len() as u32,
                )
            } != 0;
            let key = if has_guid {
                let len = guid_buffer
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(guid_buffer.len());
                String::from_utf16_lossy(&guid_buffer[..len])
            } else {
                format!("fixed-volume-{serial:08x}-{}", mount.display())
            };
            volumes.push(DiscoveredVolume {
                key: VolumeKey(key),
                mount,
                serial,
            });
        }
        start = end + 1;
    }
    volumes.sort_by(|left, right| left.mount.cmp(&right.mount));
    Ok(volumes)
}

#[cfg(not(windows))]
pub fn discover_fixed_volumes() -> Result<Vec<DiscoveredVolume>> {
    Err(AppError::Unsupported(
        "automatic fixed-volume discovery is Windows-only".to_string(),
    ))
}

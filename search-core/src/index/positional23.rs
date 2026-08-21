use super::positional_frontier;
use super::positional2::{
    Pos2BuildReport, build_segment_sidecar2_from_postings, inspect_sidecar2, sidecar2_path,
};
use super::positional3::{
    Pos3BuildReport, Pos3Policy, build_segment_sidecar3_from_postings, inspect_sidecar3,
    sidecar3_path,
};
use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct Pos23BuildReport {
    pub pos2: Pos2BuildReport,
    pub pos3: Pos3BuildReport,
    pub elapsed_ms: f64,
}

#[derive(Clone, Copy)]
struct Pos23BuildConfig {
    q3_threshold_ppm: u32,
    pos2_child_threshold_ppm: u32,
    pos3_child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    durable: bool,
}

type Pos2Stats = (u64, u64, u64, u64);
type Pos3Stats = (u64, u64, u64, [u64; 6]);

fn minimum_child(unit_count: u32, threshold_ppm: u32) -> u32 {
    u64::from(unit_count)
        .saturating_mul(u64::from(threshold_ppm))
        .div_ceil(1_000_000) as u32
}

fn publish_sidecar(path: &Path, temp_extension: String, bytes: &[u8], durable: bool) -> Result<()> {
    let tmp = path.with_extension(temp_extension);
    let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    let publish = (|| -> Result<()> {
        f.write_all(bytes)?;
        if durable {
            f.sync_all()?;
        }
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if publish.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    publish
}

fn build_one_sidecars23(
    root: &Path,
    entry: &ManifestSegment,
    index: usize,
    config: Pos23BuildConfig,
    _directory_sync_lock: &Mutex<()>,
) -> Result<(Pos2Stats, Pos3Stats)> {
    let Pos23BuildConfig {
        q3_threshold_ppm,
        pos2_child_threshold_ppm,
        pos3_child_threshold_ppm,
        max_gram,
        policy,
        durable,
    } = config;
    if q3_threshold_ppm > 1_000_000
        || pos2_child_threshold_ppm > 1_000_000
        || pos3_child_threshold_ppm > 1_000_000
        || !(4..=positional_frontier::MAX_GRAM).contains(&max_gram)
    {
        return Err(SearchError::Format(
            "invalid shared PRPOS002/PRPOS003 configuration".into(),
        ));
    }

    let segment_path = root.join(&entry.file);
    let segment = SegmentReader::open(&segment_path, false)?;
    let pos2_path = sidecar2_path(&segment_path);
    let pos3_path = sidecar3_path(&segment_path);

    let existing_pos2 = if pos2_path.exists() {
        Some(inspect_sidecar2(
            &pos2_path,
            &segment,
            q3_threshold_ppm,
            pos2_child_threshold_ppm,
        )?)
    } else {
        None
    };
    let existing_pos3 = if pos3_path.exists() {
        Some(inspect_sidecar3(
            &pos3_path,
            &segment,
            q3_threshold_ppm,
            pos3_child_threshold_ppm,
            max_gram,
            policy,
        )?)
    } else {
        None
    };
    if let (Some(pos2), Some(pos3)) = (existing_pos2, existing_pos3) {
        return Ok((pos2, pos3));
    }

    // One TEXT_BLOB candidate-start scan and one q4→qN frontier feed both persistent tiers.
    // The q8 compatibility flag retains the frozen PRPOS002 packed-key alias semantics while
    // q9+ still applies the PRPOS003 density threshold before any record is persisted.
    let dense_q3 = positional_frontier::dense_q3_keys(&segment, q3_threshold_ppm)?;
    let pos2_minimum = minimum_child(segment.unit_count(), pos2_child_threshold_ppm);
    let pos3_minimum = minimum_child(segment.unit_count(), pos3_child_threshold_ppm);
    let frontier_max = max_gram.max(8);
    let (keys, postings) = positional_frontier::build_shared_postings(
        &segment,
        &dense_q3,
        pos2_minimum,
        pos3_minimum,
        frontier_max,
        true,
    )?;

    let mut published = false;
    if existing_pos2.is_none() {
        let bytes = build_segment_sidecar2_from_postings(
            &segment,
            q3_threshold_ppm,
            pos2_child_threshold_ppm,
            &keys,
            &postings,
        )?;
        publish_sidecar(
            &pos2_path,
            format!("pos2.tmp-{}-{index}", std::process::id()),
            &bytes,
            durable,
        )?;
        published = true;
    }
    if existing_pos3.is_none() {
        let bytes = build_segment_sidecar3_from_postings(
            &segment,
            q3_threshold_ppm,
            pos3_child_threshold_ppm,
            max_gram,
            policy,
            &keys,
            &postings,
        )?;
        publish_sidecar(
            &pos3_path,
            format!("pos3.tmp-{}-{index}", std::process::id()),
            &bytes,
            durable,
        )?;
        published = true;
    }
    if durable && published {
        #[cfg(unix)]
        {
            let _guard = _directory_sync_lock.lock().map_err(|_| {
                SearchError::Format("PRPOS002/PRPOS003 directory sync lock poisoned".into())
            })?;
            fs::File::open(root)?.sync_all()?;
        }
    }

    let pos2 = match existing_pos2 {
        Some(stats) => stats,
        None => inspect_sidecar2(
            &pos2_path,
            &segment,
            q3_threshold_ppm,
            pos2_child_threshold_ppm,
        )?,
    };
    let pos3 = match existing_pos3 {
        Some(stats) => stats,
        None => inspect_sidecar3(
            &pos3_path,
            &segment,
            q3_threshold_ppm,
            pos3_child_threshold_ppm,
            max_gram,
            policy,
        )?,
    };
    Ok((pos2, pos3))
}

pub fn build_positional23_sidecars(
    root: impl AsRef<Path>,
    q3_threshold_ppm: u32,
    pos2_child_threshold_ppm: u32,
    pos3_child_threshold_ppm: u32,
    max_gram: usize,
    policy: Pos3Policy,
    durable: bool,
) -> Result<Pos23BuildReport> {
    let started = Instant::now();
    let root = root.as_ref();
    let (docs, manifest) = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(docs, &manifest)?;
    if manifest.is_empty() {
        return Ok(Pos23BuildReport::default());
    }

    let workers = manifest.len().clamp(1, 4);
    let next = AtomicUsize::new(0);
    let directory_sync_lock = Mutex::new(());
    let config = Pos23BuildConfig {
        q3_threshold_ppm,
        pos2_child_threshold_ppm,
        pos3_child_threshold_ppm,
        max_gram,
        policy,
        durable,
    };
    type R = Option<Result<(Pos2Stats, Pos3Stats)>>;
    let results: Arc<Mutex<Vec<R>>> = Arc::new(Mutex::new(
        std::iter::repeat_with(|| None)
            .take(manifest.len())
            .collect(),
    ));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let results = Arc::clone(&results);
            let next = &next;
            let manifest = &manifest;
            let directory_sync_lock = &directory_sync_lock;
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if i >= manifest.len() {
                        break;
                    }
                    let r =
                        build_one_sidecars23(root, &manifest[i], i, config, directory_sync_lock);
                    results
                        .lock()
                        .expect("PRPOS002/PRPOS003 build result poisoned")[i] = Some(r);
                }
            });
        }
    });

    let mut report = Pos23BuildReport::default();
    for result in Arc::try_unwrap(results)
        .map_err(|_| SearchError::Format("PRPOS002/PRPOS003 result ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("PRPOS002/PRPOS003 result mutex poisoned".into()))?
    {
        let (pos2, pos3) = result
            .ok_or_else(|| SearchError::Format("PRPOS002/PRPOS003 segment not built".into()))??;
        report.pos2.segments += 1;
        report.pos2.bytes += pos2.0;
        report.pos2.records += pos2.1;
        report.pos2.units += pos2.2;
        report.pos2.occurrences += pos2.3;

        report.pos3.segments += 1;
        report.pos3.bytes += pos3.0;
        report.pos3.records += pos3.1;
        report.pos3.units += pos3.2;
        report.pos3.delta_records += pos3.3[0];
        report.pos3.bitmap_records += pos3.3[1];
        report.pos3.complement_records += pos3.3[2];
        report.pos3.all_records += pos3.3[3];
        report.pos3.run_records += pos3.3[4];
        report.pos3.bp128_records += pos3.3[5];
    }
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

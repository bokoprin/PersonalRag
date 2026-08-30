use super::super::DiscoveredVolume;
#[cfg(windows)]
use super::super::VolumeKey;
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

pub(super) struct RuntimeWatchers {
    enabled: bool,
    #[cfg(windows)]
    slots: HashMap<VolumeKey, WatchSlot>,
}

#[cfg(windows)]
struct WatchSlot {
    notification: Option<crate::windows_watch::live::ChangeNotification>,
    last_fallback: Instant,
}

impl RuntimeWatchers {
    pub(super) fn new(volumes: &[DiscoveredVolume], enabled: bool) -> Self {
        #[cfg(windows)]
        {
            let mut slots = HashMap::new();
            if enabled {
                for volume in volumes {
                    slots.insert(
                        volume.key.clone(),
                        WatchSlot {
                            notification: crate::windows_watch::live::ChangeNotification::open(
                                &volume.mount,
                            )
                            .ok(),
                            last_fallback: Instant::now(),
                        },
                    );
                }
            }
            Self { enabled, slots }
        }
        #[cfg(not(windows))]
        {
            let _ = volumes;
            Self { enabled }
        }
    }

    pub(super) fn should_refresh(&mut self, volume: &DiscoveredVolume) -> bool {
        if !self.enabled {
            return false;
        }
        #[cfg(windows)]
        {
            let Some(slot) = self.slots.get_mut(&volume.key) else {
                return false;
            };
            if let Some(notification) = slot.notification.as_ref() {
                match notification.poll_changed() {
                    Ok(true) => {
                        slot.last_fallback = Instant::now();
                        return true;
                    }
                    Ok(false) => return false,
                    Err(_) => {
                        slot.notification = None;
                        slot.last_fallback = Instant::now() - FALLBACK_RECONCILE_INTERVAL;
                    }
                }
            }
            if slot.last_fallback.elapsed() >= FALLBACK_RECONCILE_INTERVAL {
                slot.last_fallback = Instant::now();
                return true;
            }
            false
        }
        #[cfg(not(windows))]
        {
            let _ = volume;
            false
        }
    }

    pub(super) fn reopen(&mut self, volume: &DiscoveredVolume) {
        if !self.enabled {
            return;
        }
        #[cfg(windows)]
        {
            let Some(slot) = self.slots.get_mut(&volume.key) else {
                return;
            };
            if slot.notification.is_none() {
                slot.notification =
                    crate::windows_watch::live::ChangeNotification::open(&volume.mount).ok();
            }
            slot.last_fallback = Instant::now();
        }
        #[cfg(not(windows))]
        {
            let _ = volume;
        }
    }
}

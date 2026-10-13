//! In-process artifact ownership, shared by readers, recovery and cleanup.
use std::{
    collections::{BinaryHeap, HashMap},
    fs::{self, File, ReadDir},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
static LEASES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
pub(super) struct Lease(PathBuf);
fn key(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or("artifact path invalid")?
        .canonicalize()
        .map_err(|_| "artifact directory unavailable")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("artifact path invalid")?;
    Ok(parent.join(name.strip_suffix(".part").unwrap_or(name)))
}
impl Lease {
    pub(super) fn acquire(path: &Path) -> Result<Self, String> {
        let path = key(path)?;
        *LEASES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(path.clone())
            .or_default() += 1;
        Ok(Self(path))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut all = LEASES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(n) = all.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                all.remove(&self.0);
            }
        }
    }
}
fn leased(path:&Path)->bool {
    let Ok(key)=key(path) else{return true};
    LEASES.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).contains_key(&key)
}
pub(super) fn remove_unleased(root: &Path, path: &Path) -> bool {
    let Ok(key) = key(path) else { return false };
    let all = LEASES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if all.contains_key(&key) {
        return false;
    }
    // Atomic with lease acquisition, including the cross-process frame lease.
    if fs::symlink_metadata(path).map_or(true, |m| !m.is_file() || m.file_type().is_symlink()) {
        return false;
    }
    super::video_frames::remove_if_unleased(root, path)
}
pub(super) struct CachedFile {
    file: File,
    _lease: Lease,
}
impl CachedFile {
    pub(super) fn open(path: &Path) -> Result<(Self, u64), String> {
        let lease = Lease::acquire(path)?;
        if fs::symlink_metadata(path).map_or(true, |m| !m.is_file() || m.file_type().is_symlink()) {
            return Err("artifact unavailable".into());
        }
        let file = File::open(path).map_err(|_| "artifact unavailable")?;
        let size = file.metadata().map_err(|_| "artifact unavailable")?.len();
        if size == 0 || size > super::video_store::MAX_VIDEO_BYTES {
            return Err("artifact length invalid".into());
        }
        Ok((
            Self {
                file,
                _lease: lease,
            },
            size,
        ))
    }
}
impl Read for CachedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub bytes: u64,
    pub files: usize,
}
pub(super) const LIMITS: Limits = Limits {
    bytes: 64 * 1024 * 1024 * 1024,
    files: 2048,
};
const DISK_RESERVE: u64 = 8 * 1024 * 1024 * 1024;
fn check_disk_space(available: u64, reserved: u64, incoming: u64) -> Result<(), String> {
    let required = reserved
        .checked_add(incoming)
        .and_then(|n| n.checked_add(DISK_RESERVE))
        .ok_or("artifact_cache_full")?;
    if available < required {
        Err("artifact_cache_full".into())
    } else {
        Ok(())
    }
}
#[cfg(windows)]
fn free_space(path: &Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err("artifact_disk_space_unavailable".into());
    }
    Ok(available)
}
#[cfg(not(windows))]
fn free_space(_path: &Path) -> Result<u64, String> {
    Err("artifact_disk_space_unavailable".into())
}
static RESERVED: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
pub(super) struct Reservation(PathBuf);
fn artifact(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".mp4") || n.ends_with(".mp4.part"))
}
impl Reservation {
    pub(super) fn acquire(
        root: &Path,
        path: &Path,
        bytes: u64,
        limits: Limits,
    ) -> Result<Self, String> {
        Self::acquire_with_space(root,path,bytes,limits,&free_space)
    }
    fn acquire_with_space(root:&Path,path:&Path,bytes:u64,limits:Limits,space:&impl Fn(&Path)->Result<u64,String>)->Result<Self,String> {
        let path = key(path)?;
        let dir = super::video_store::storage_dir(root)
            .canonicalize()
            .map_err(|_| "artifact directory unavailable")?;
        if path.parent() != Some(dir.as_path()) {
            return Err("artifact path invalid".into());
        }
        let mut reserved = RESERVED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if reserved.contains_key(&path) {
            return Err("artifact_recovery_busy".into());
        }
        let mut total = bytes;
        let mut count = 1usize;
        for (_, n) in reserved
            .iter()
            .filter(|(p, _)| p.parent() == Some(dir.as_path()))
        {
            total = total.saturating_add(*n);
            count += 1;
        }
        let pending=total.saturating_sub(bytes);
        if total>limits.bytes || count>limits.files {return Err("artifact_cache_full".into());}
        check_disk_space(u64::MAX,pending,bytes)?; // reject impossible/overflowed demand without evicting
        let mut available=space(&dir)?;
        let mut candidates=BinaryHeap::new();
        // Bound admission inventory too; excessive directories wait for cleanup.
        for (index, entry) in fs::read_dir(&dir)
            .map_err(|_| "artifact cache unavailable")?
            .enumerate()
        {
            if index >= 4096 {
                return Err("artifact_cache_full".into());
            }
            let entry = entry.map_err(|_| "artifact cache unavailable")?;
            let candidate = entry.path();
            if !artifact(&candidate) {
                continue;
            }
            let metadata =
                fs::symlink_metadata(&candidate).map_err(|_| "artifact cache unavailable")?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            if candidate.extension().and_then(|x| x.to_str()) == Some("part")
                && reserved.contains_key(&key(&candidate)?)
            {
                continue;
            }
            total = total.saturating_add(metadata.len());
            count += 1;
            // Active entries do not occupy the bounded candidate set. Recheck
            // under the same lease lock at removal, since ownership can change.
            if !leased(&candidate) && !reserved.contains_key(&key(&candidate)?) {
                let modified=metadata.modified().ok().and_then(|t|t.duration_since(UNIX_EPOCH).ok()).map_or(u64::MAX,|d|d.as_secs());
                candidates.push((modified,candidate,metadata.len()));if candidates.len()>128 {candidates.pop();}
            }
        }
        // The new reservation, including other not-yet-published downloads,
        // drives eviction even when current inventory is exactly at its limit
        // or free space is still above the idle maintenance watermark.
        for (_,candidate,size) in candidates.into_sorted_vec() {
            if total<=limits.bytes && count<=limits.files && check_disk_space(available,pending,bytes).is_ok(){break;}
            if remove_unleased(root,&candidate) {
                total=total.saturating_sub(size);count=count.saturating_sub(1);
                // Actual free space may differ from logical file length (sparse
                // files, delayed unlink, outside writers). Never invent headroom.
                available=space(&dir)?;
            }
        }
        if total > limits.bytes || count > limits.files || check_disk_space(available,pending,bytes).is_err() {
            return Err("artifact_cache_full".into());
        }
        reserved.insert(path.clone(), bytes);
        Ok(Self(path))
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}
#[derive(Default)]
pub(super) struct Sweep {
    scan: Option<ReadDir>,
    bytes: u64,
    files: usize,
    oldest: BinaryHeap<(u64, PathBuf, u64)>,
}
impl Sweep {
    pub(super) fn step(&mut self, root: &Path, retention: u64, limits: Limits) -> usize {
        if self.scan.is_none() {
            self.bytes = 0;
            self.files = 0;
            self.oldest.clear();
            self.scan = fs::read_dir(super::video_store::storage_dir(root)).ok();
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut removed = 0;
        let mut ended = false;
        for _ in 0..256 {
            let Some(entry) = self.scan.as_mut().and_then(Iterator::next) else {
                ended = true;
                break;
            };
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !artifact(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(now, |d| d.as_secs());
            if removed < 128
                && retention > 0
                && now.saturating_sub(modified) >= retention
                && remove_unleased(root, &path)
            {
                removed += 1;
                continue;
            }
            self.bytes = self.bytes.saturating_add(meta.len());
            self.files += 1;
            if !leased(&path) {
                self.oldest.push((modified, path, meta.len()));
                if self.oldest.len() > 128 {
                    self.oldest.pop();
                }
            }
        }
        let mut available = free_space(&super::video_store::storage_dir(root)).unwrap_or(u64::MAX);
        if ended || available < DISK_RESERVE {
            for (_, path, size) in std::mem::take(&mut self.oldest).into_sorted_vec() {
                if removed >= 128
                    || self.bytes <= limits.bytes
                        && self.files <= limits.files
                        && available >= DISK_RESERVE
                {
                    break;
                }
                if remove_unleased(root, &path) {
                    self.bytes = self.bytes.saturating_sub(size);
                    self.files = self.files.saturating_sub(1);
                    available = available.saturating_add(size);
                    removed += 1;
                }
            }
            if ended {
                self.scan = None;
            }
        }
        removed
    }
}
pub(super) fn start(root: PathBuf, stop: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut sweep = Sweep::default();
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if stop.load(Ordering::Acquire) {
                break;
            }
            let root = root.clone();
            match tokio::task::spawn_blocking(move || {
                sweep.step(&root, 86400, LIMITS);
                sweep
            })
            .await
            {
                Ok(next) => sweep = next,
                Err(_) => {
                    eprintln!("video cache maintenance unavailable");
                    sweep = Sweep::default();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_cleanup_keeps_readers_downloads_and_extraction_until_real_exit() {
        let root = std::env::temp_dir().join(format!(
            "aiwork-cache-lease-{:032x}",
            rand::random::<u128>()
        ));
        let dir = super::super::video_store::storage_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("active.mp4");
        let partial = dir.join("active.mp4.part");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&partial, b"inflight").unwrap();
        let lease = Lease::acquire(&path).unwrap();
        let second = Lease::acquire(&partial).unwrap();
        assert!(
            !remove_unleased(&root, &path),
            "cleanup must preserve an active reader"
        );
        assert!(
            !remove_unleased(&root, &partial),
            "partial and publication share ownership"
        );
        drop(lease);
        assert!(!remove_unleased(&root, &path));
        drop(second);
        assert!(remove_unleased(&root, &path));
        assert!(remove_unleased(&root, &partial));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_cleanup_is_batched_and_continues_without_restart() {
        let root = std::env::temp_dir().join(format!(
            "aiwork-cache-sweep-{:032x}",
            rand::random::<u128>()
        ));
        let dir = super::super::video_store::storage_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..300 {
            let file = std::fs::File::create(dir.join(format!("old-{i}.mp4"))).unwrap();
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        }
        let mut sweep = Sweep::default();
        let removed = sweep.step(&root, 86400, LIMITS);
        assert!(
            removed > 0 && removed <= 256,
            "each maintenance tick must have a bounded batch, got {removed}"
        );
        for _ in 0..4 {
            sweep.step(&root, 86400, LIMITS);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_capacity_accounts_for_parallel_unpublished_downloads() {
        let root =
            std::env::temp_dir().join(format!("aiwork-cache-cap-{:032x}", rand::random::<u128>()));
        let dir = super::super::video_store::storage_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let limits = Limits {
            bytes: 10,
            files: 2,
        };
        let first = Reservation::acquire(&root, &dir.join("a.mp4"), 8, limits).unwrap();
        assert!(
            Reservation::acquire(&root, &dir.join("b.mp4"), 3, limits).is_err(),
            "in-flight bytes must reserve capacity before files appear"
        );
        drop(first);
        assert!(Reservation::acquire(&root, &dir.join("b.mp4"), 3, limits).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_capacity_sweep_skips_active_file_and_evicts_an_unleased_file() {
        let root = std::env::temp_dir().join(format!(
            "aiwork-cache-evict-{:032x}",
            rand::random::<u128>()
        ));
        let dir = super::super::video_store::storage_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.mp4");
        let second = dir.join("second.mp4");
        std::fs::write(&first, b"123456").unwrap();
        std::fs::write(&second, b"123456").unwrap();
        let (mut reader, size) = CachedFile::open(&first).unwrap();
        assert_eq!(size, 6);
        let limits = Limits {
            bytes: 10,
            files: 10,
        };
        assert_eq!(Sweep::default().step(&root, 0, limits), 1);
        assert!(first.exists());
        assert!(!second.exists());
        let mut contents = String::new();
        reader.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "123456");
        assert!(Reservation::acquire(&root, &dir.join("third.mp4"), 3, limits).is_ok());
        drop(reader);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_disk_low_water_counts_inflight_reservations_before_admission() {
        let gib = 1024 * 1024 * 1024;
        assert!(
            check_disk_space(12 * gib, 3 * gib, 2 * gib).is_err(),
            "leave 8 GiB after pending and new writes"
        );
        assert!(check_disk_space(8 * gib, 0, 1).is_err());
        assert!(check_disk_space(13 * gib, 3 * gib, 2 * gib).is_ok());
        assert!(
            check_disk_space(u64::MAX, u64::MAX, 1).is_err(),
            "overflow must not invent space"
        );
    }
    #[test]
    fn cache_demand_admits_at_file_limit_without_waiting_for_retention() {
        let root=std::env::temp_dir().join(format!("aiwork-cache-demand-files-{:032x}",rand::random::<u128>()));
        let dir=super::super::video_store::storage_dir(&root);fs::create_dir_all(&dir).unwrap();
        let active=dir.join("active.mp4");let idle=dir.join("idle.mp4");fs::write(&active,b"a").unwrap();fs::write(&idle,b"b").unwrap();
        let held=Lease::acquire(&active).unwrap();
        let admitted=Reservation::acquire(&root,&dir.join("new.mp4"),1,Limits {bytes:100,files:2});
        assert!(admitted.is_ok(),"new demand must reclaim an idle file at the equality boundary");
        assert!(active.exists());assert!(!idle.exists());drop(admitted);drop(held);fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_demand_reclaims_disk_headroom_for_new_and_pending_downloads() {
        let root=std::env::temp_dir().join(format!("aiwork-cache-demand-space-{:032x}",rand::random::<u128>()));
        let dir=super::super::video_store::storage_dir(&root);fs::create_dir_all(&dir).unwrap();let idle=dir.join("idle.mp4");fs::write(&idle,b"12").unwrap();
        let space=|_:&Path|Ok(DISK_RESERVE+if idle.exists(){9}else{11});let limits=Limits {bytes:100,files:10};
        let first=Reservation::acquire_with_space(&root,&dir.join("first.mp4"),4,limits,&space).unwrap();assert!(idle.exists());
        let second=Reservation::acquire_with_space(&root,&dir.join("second.mp4"),6,limits,&space);
        assert!(second.is_ok(),"free space above the watermark can still require demand-driven eviction");assert!(!idle.exists());
        assert!(Reservation::acquire_with_space(&root,&dir.join("third.mp4"),2,limits,&space).is_err(),"accepted reservations remain exclusive");
        drop(second);drop(first);fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_demand_eviction_is_bounded_and_retry_makes_progress() {
        let root=std::env::temp_dir().join(format!("aiwork-cache-demand-batch-{:032x}",rand::random::<u128>()));
        let dir=super::super::video_store::storage_dir(&root);fs::create_dir_all(&dir).unwrap();for i in 0..200 {fs::write(dir.join(format!("file-{i}.mp4")),b"x").unwrap();}
        let limits=Limits {bytes:1000,files:1};let target=dir.join("new.mp4");assert!(Reservation::acquire(&root,&target,1,limits).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(),72,"one attempt removes at most 128 candidates");
        assert!(Reservation::acquire(&root,&target,1,limits).is_ok(),"the next bounded attempt completes admission");fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_demand_sweep_passes_a_full_oldest_page_of_active_files() {
        let root=std::env::temp_dir().join(format!("aiwork-cache-demand-leases-{:032x}",rand::random::<u128>()));
        let dir=super::super::video_store::storage_dir(&root);fs::create_dir_all(&dir).unwrap();let mut leases=Vec::new();
        for i in 0..128 {let path=dir.join(format!("active-{i}.mp4"));fs::write(&path,b"a").unwrap();File::options().write(true).open(&path).unwrap().set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH+Duration::from_secs(1))).unwrap();leases.push(Lease::acquire(&path).unwrap());}
        let idle=dir.join("newer-idle.mp4");fs::write(&idle,b"b").unwrap();let mut sweep=Sweep::default();
        assert_eq!(sweep.step(&root,0,Limits {bytes:128,files:128}),1,"active oldest entries must not hide eligible later files");
        assert!(!idle.exists());assert_eq!(fs::read_dir(&dir).unwrap().count(),128);drop(leases);fs::remove_dir_all(root).unwrap();
    }
}

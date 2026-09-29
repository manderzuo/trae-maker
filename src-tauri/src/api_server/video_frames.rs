//! Private, bounded extraction from an already authenticated local artifact.
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
const MAX_FRAME: u64 = 8 * 1024 * 1024;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const ALGORITHM: &str = "last-decodable-tail2-v1";
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrameArtifact {
    pub path: PathBuf,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub timestamp_ms: i64,
    pub source_sha256: String,
    pub frame_sha256: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractorConfig {
    path: PathBuf,
    sha256: String,
}
fn hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(crate) fn read_small(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let f = File::open(path).map_err(|_| "frame_output_unavailable")?;
    let size = f.metadata().map_err(|_| "frame_output_unavailable")?.len();
    if size > max {
        return Err("frame_output_too_large".into());
    }
    let mut bytes = Vec::new();
    f.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "frame_output_unavailable")?;
    if bytes.len() as u64 > max || bytes.len() as u64 != size {
        return Err("frame_output_invalid".into());
    }
    Ok(bytes)
}
fn digest_file(path: &Path, max: u64) -> Result<String, String> {
    let mut f = File::open(path).map_err(|_| "frame_source_unavailable")?;
    let len = f.metadata().map_err(|_| "frame_source_unavailable")?.len();
    if len == 0 || len > max {
        return Err("frame_source_invalid".into());
    }
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    let mut read = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|_| "frame_source_unavailable")?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > max {
            return Err("frame_source_invalid".into());
        }
        hash.update(&buf[..n]);
    }
    if read != len {
        return Err("frame_source_changed".into());
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub(crate) fn configured_extractor(data_dir: &Path) -> Result<PathBuf, String> {
    let bytes = read_small(&data_dir.join("video-frame-extractor.json"), 8192)
        .map_err(|_| "frame_extractor_unconfigured")?;
    if bytes.len() > 8192 {
        return Err("frame_extractor_unconfigured".into());
    }
    let c: ExtractorConfig =
        serde_json::from_slice(&bytes).map_err(|_| "frame_extractor_unconfigured")?;
    if !c.path.is_absolute()
        || !hex(&c.sha256)
        || fs::symlink_metadata(&c.path)
            .map_err(|_| "frame_extractor_unavailable")?
            .file_type()
            .is_symlink()
    {
        return Err("frame_extractor_unavailable".into());
    }
    if digest_file(&c.path, 256 * 1024 * 1024)? != c.sha256.to_ascii_lowercase() {
        return Err("frame_extractor_digest_mismatch".into());
    }
    Ok(c.path)
}
#[derive(Default)]
struct Counts {
    active: usize,
    queued: usize,
}
struct Limits {
    counts: Mutex<Counts>,
    ready: Condvar,
}
static LIMITS: OnceLock<std::sync::Arc<Limits>> = OnceLock::new();
fn limits() -> std::sync::Arc<Limits> {
    LIMITS
        .get_or_init(|| {
            std::sync::Arc::new(Limits {
                counts: Mutex::new(Counts::default()),
                ready: Condvar::new(),
            })
        })
        .clone()
}
pub(super) struct Slot {
    limits: std::sync::Arc<Limits>,
}
impl Slot {
    pub(super) fn acquire() -> Result<Self, String> {
        Self::on(limits())
    }
    fn on(l: std::sync::Arc<Limits>) -> Result<Self, String> {
        let mut c = l.counts.lock().unwrap_or_else(|e| e.into_inner());
        if c.active < 2 {
            c.active += 1;
            drop(c);
            return Ok(Self { limits: l });
        }
        if c.queued >= 8 {
            return Err("frame_extractor_busy".into());
        }
        c.queued += 1;
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                c.queued -= 1;
                return Err("frame_extractor_busy".into());
            }
            c = l
                .ready
                .wait_timeout(c, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            if c.active < 2 {
                c.queued -= 1;
                c.active += 1;
                drop(c);
                return Ok(Self { limits: l });
            }
        }
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let l = &self.limits;
        let mut c = l.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.active = c.active.saturating_sub(1);
        l.ready.notify_one();
    }
}

// Durable expiry makes an interrupted extractor recoverable after restart.
// The same short lock covers lease publication and cleanup's remove decision.
static MEDIA_LOCK: Mutex<()> = Mutex::new(());
pub(crate) struct MediaLease {
    path: PathBuf,
}
fn lease_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("data/video-frame-leases")
}
fn path_digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()))
}
impl MediaLease {
    pub(crate) fn acquire(data_dir: &Path, source: &Path) -> Result<Self, String> {
        let _lock = MEDIA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let canonical = source
            .canonicalize()
            .map_err(|_| "frame_source_unavailable")?;
        let dir = lease_dir(data_dir);
        fs::create_dir_all(&dir).map_err(|_| "frame_lease_unavailable")?;
        let path = dir.join(format!(
            "{}-{:032x}.json",
            path_digest(&canonical),
            rand::random::<u128>()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|_| "frame_lease_unavailable")?;
        let expires = chrono::Utc::now().timestamp_millis() + 120000;
        if file
            .write_all(expires.to_string().as_bytes())
            .and_then(|_| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&path);
            return Err("frame_lease_unavailable".into());
        }
        Ok(Self { path })
    }
}
impl Drop for MediaLease {
    fn drop(&mut self) {
        let _lock = MEDIA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = fs::remove_file(&self.path);
    }
}
pub(crate) fn remove_if_unleased(data_dir: &Path, source: &Path) -> bool {
    let _lock = MEDIA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(canonical) = source.canonicalize() else {
        return false;
    };
    let prefix = format!("{}-", path_digest(&canonical));
    if let Ok(entries) = fs::read_dir(lease_dir(data_dir)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            if entry.file_type().map_or(true, |t| !t.is_file()) {
                return false;
            }
            let expiry = read_small(&entry.path(), 32)
                .ok()
                .and_then(|v| String::from_utf8(v).ok())
                .and_then(|v| v.parse::<i64>().ok());
            if expiry.map_or(true, |v| v > chrono::Utc::now().timestamp_millis()) {
                return false;
            }
            let _ = fs::remove_file(entry.path());
        }
    }
    fs::remove_file(source).is_ok()
}
fn png(path: &Path) -> Result<(u32, u32, String), String> {
    let bytes = read_small(path, MAX_FRAME)?;
    if bytes.len() < 45
        || bytes.len() as u64 > MAX_FRAME
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || &bytes[12..16] != b"IHDR"
        || &bytes[bytes.len() - 8..bytes.len() - 4] != b"IEND"
    {
        return Err("frame_output_invalid".into());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 8388608 {
        return Err("frame_output_invalid".into());
    }
    Ok((width, height, format!("{:x}", Sha256::digest(&bytes))))
}
fn cached(path: &Path, meta: &Path, source: &str) -> Option<FrameArtifact> {
    if fs::symlink_metadata(path).ok()?.file_type().is_symlink()
        || fs::metadata(meta).ok()?.len() > 8192
    {
        return None;
    }
    let mut frame: FrameArtifact = serde_json::from_slice(&read_small(meta, 8192).ok()?).ok()?;
    let (w, h, d) = png(path).ok()?;
    if frame.source_sha256 != source
        || frame.frame_sha256 != d
        || frame.width != w
        || frame.height != h
        || frame.timestamp_ms < 0
        || frame.mime != "image/png"
    {
        return None;
    }
    frame.path = path.into();
    Some(frame)
}
pub(crate) fn extract_last(
    data_dir: &Path,
    owned_video_path: &Path,
    extractor_path: &Path,
) -> Result<FrameArtifact, String> {
    extract_with_timeout(data_dir, owned_video_path, extractor_path, PROCESS_TIMEOUT)
}
/// Authenticated uploaded bytes, never a caller-supplied local path or URL.
/// The caller holds a bounded upload permit until this worker exits.
pub(crate) fn extract_uploaded(data_dir: &Path, bytes: &[u8]) -> Result<FrameArtifact,String> {
    if !(12..=32*1024*1024).contains(&bytes.len()) || &bytes[4..8]!=b"ftyp" {
        return Err("frame_source_invalid".into());
    }
    let extractor=configured_extractor(data_dir)?;
    let root=super::video_store::storage_dir(data_dir);
    fs::create_dir_all(&root).map_err(|_|"frame_source_unavailable")?;
    let path=root.join(format!("reference-frame-{:032x}.mp4",rand::random::<u128>()));
    struct Source(PathBuf);
    impl Drop for Source {fn drop(&mut self) {let _=fs::remove_file(&self.0);}}
    let mut file=OpenOptions::new().create_new(true).write(true).open(&path).map_err(|_|"frame_source_unavailable")?;
    let source=Source(path);
    let written=file.write_all(bytes).and_then(|_|file.sync_all());
    drop(file);
    written.map_err(|_|"frame_source_unavailable")?;
    extract_last(data_dir,&source.0,&extractor)
}
fn extract_with_timeout(
    data_dir: &Path,
    source: &Path,
    extractor: &Path,
    timeout: Duration,
) -> Result<FrameArtifact, String> {
    let allowed = configured_extractor(data_dir)?;
    if allowed.canonicalize().ok() != extractor.canonicalize().ok() {
        return Err("frame_extractor_unavailable".into());
    }
    let meta = fs::symlink_metadata(source).map_err(|_| "frame_source_unavailable")?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || source.extension().and_then(|v| v.to_str()) != Some("mp4")
    {
        return Err("frame_source_invalid".into());
    }
    let canonical = source
        .canonicalize()
        .map_err(|_| "frame_source_unavailable")?;
    let root = super::video_store::storage_dir(data_dir)
        .canonicalize()
        .map_err(|_| "frame_source_unavailable")?;
    if canonical.parent() != Some(root.as_path()) {
        return Err("frame_source_invalid".into());
    }
    let _slot = Slot::acquire()?;
    let _lease = MediaLease::acquire(data_dir, &canonical)?;
    let source_sha = digest_file(&canonical, super::video_store::MAX_VIDEO_BYTES)?;
    let exe_sha = digest_file(&allowed, 256 * 1024 * 1024)?;
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{ALGORITHM}:{source_sha}:{exe_sha}"))
    );
    let dir = data_dir.join("data/video-frames");
    fs::create_dir_all(&dir).map_err(|_| "frame_output_unavailable")?;
    let path = dir.join(format!("{key}.png"));
    let metadata = dir.join(format!("{key}.json"));
    if let Some(frame) = cached(&path, &metadata, &source_sha) {
        return Ok(frame);
    }
    let partial = dir.join(format!("{key}-{:032x}.png", rand::random::<u128>()));
    let mut command = Command::new(&allowed);
    command
        .args([
            "-hide_banner",
            "-nostdin",
            "-protocol_whitelist",
            "file,pipe",
            "-sseof",
            "-2",
            "-copyts",
            "-threads",
            "1",
            "-max_pixels",
            "8388608",
            "-i",
        ])
        .arg(&canonical)
        .args([
            "-an",
            "-sn",
            "-vf",
            "showinfo",
            "-threads",
            "1",
            "-c:v",
            "png",
            "-update",
            "1",
            "-atomic_writing",
            "1",
            "-y",
        ])
        .arg(&partial)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| "frame_extractor_unavailable")?;
    let mut stderr = child.stderr.take().ok_or("frame_extractor_unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut b = [0; 4096];
        while let Ok(n) = stderr.read(&mut b) {
            if n == 0 {
                break;
            }
            output.extend_from_slice(&b[..n]);
            if output.len() > 65536 {
                output.drain(..output.len() - 65536);
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    });
    let started = Instant::now();
    let outcome = loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break Err("frame_extraction_timeout");
        }
        if fs::metadata(&partial).is_ok_and(|m| m.len() > MAX_FRAME) {
            let _ = child.kill();
            let _ = child.wait();
            break Err("frame_output_too_large");
        }
        match child.try_wait() {
            Ok(Some(s)) => {
                break if s.success() {
                    Ok(())
                } else {
                    Err("frame_extraction_failed")
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("frame_extraction_failed");
            }
        }
    };
    let log = reader.join().unwrap_or_default();
    let publish = (|| {
        outcome.map_err(str::to_owned)?;
        let (width, height, frame_sha) = png(&partial)?;
        let timestamp = log
            .lines()
            .filter_map(|l| l.split_once("pts_time:").map(|(_, r)| r))
            .filter_map(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 86400.0)
            .last()
            .ok_or("frame_timestamp_invalid")?;
        if digest_file(&canonical, super::video_store::MAX_VIDEO_BYTES)? != source_sha {
            return Err("frame_source_changed".into());
        }
        let frame = FrameArtifact {
            path: path.clone(),
            mime: "image/png".into(),
            width,
            height,
            timestamp_ms: (timestamp * 1000.0).round() as i64,
            source_sha256: source_sha.clone(),
            frame_sha256: frame_sha,
        };
        // No input path, URL or subprocess output is returned to callers.
        if path.exists() {
            if let Some(existing) = cached(&path, &metadata, &source_sha) {
                return Ok(existing);
            }
            fs::remove_file(&path).map_err(|_| "frame_output_unavailable")?;
        }
        fs::rename(&partial, &path).map_err(|_| "frame_output_unavailable")?;
        let temp = dir.join(format!("{key}-{:032x}.json", rand::random::<u128>()));
        let saved: Result<(), String> = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "frame_output_unavailable")?;
            f.write_all(&serde_json::to_vec(&frame).map_err(|_| "frame_output_invalid")?)
                .and_then(|_| f.sync_all())
                .map_err(|_| "frame_output_unavailable")?;
            if metadata.exists() {
                fs::remove_file(&metadata).map_err(|_| "frame_output_unavailable")?;
            }
            fs::rename(&temp, &metadata).map_err(|_| "frame_output_unavailable")?;
            Ok(())
        })();
        let _ = fs::remove_file(temp);
        saved?;
        Ok(frame)
    })();
    let _ = fs::remove_file(&partial);
    let _ = fs::remove_file(partial.with_extension("png.tmp"));
    publish
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command};
    pub(crate) struct Fixture(pub(crate) PathBuf, pub(crate) PathBuf);
    impl Fixture {
        pub(crate) fn new() -> Self {
            assert!(
                std::env::var_os("AIWORK_VIDEO_DIR").is_none(),
                "tests must not use a runtime video directory"
            );
            let root =
                std::env::temp_dir().join(format!("video-frames-{:032x}", rand::random::<u128>()));
            let exe = PathBuf::from("C:/Program Files/SteelSeries/GG/apps/moments/ffmpeg.exe");
            std::fs::create_dir_all(super::super::video_store::storage_dir(&root)).unwrap();
            std::fs::write(root.join("video-frame-extractor.json"),serde_json::to_vec(&serde_json::json!({"path":exe,"sha256":"ff8d9e4fb41c9563022e2e3d4fc040130efb31595959bfc6613f1ae52f039d31"})).unwrap()).unwrap();
            let clip = super::super::video_store::artifact_path(&root, "synthetic").unwrap();
            assert!(Command::new(&exe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:s=32x32:r=8:d=1",
                    "-an",
                    "-c:v",
                    "mjpeg",
                    "-threads",
                    "1",
                    "-y"
                ])
                .arg(&clip)
                .status()
                .unwrap()
                .success());
            Self(root, exe)
        }
        pub(crate) fn clip(&self) -> PathBuf {
            super::super::video_store::artifact_path(&self.0, "synthetic").unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn short_clip_returns_last_decodable_frame() {
        let f = Fixture::new();
        let frame = extract_last(&f.0, &f.clip(), &f.1).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.timestamp_ms),
            (32, 32, 875)
        );
        assert_eq!(frame.mime, "image/png");
        let bytes = std::fs::read(&frame.path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            frame.frame_sha256,
            digest_file(&frame.path, 8 * 1024 * 1024).unwrap()
        );
    }
    #[test]
    fn same_source_digest_reuses_frame() {
        let f = Fixture::new();
        let a = extract_last(&f.0, &f.clip(), &f.1).unwrap();
        let modified = std::fs::metadata(&a.path).unwrap().modified().unwrap();
        let b = extract_last(&f.0, &f.clip(), &f.1).unwrap();
        assert_eq!(a.path, b.path);
        assert_eq!(a.frame_sha256, b.frame_sha256);
        assert_eq!(
            std::fs::metadata(b.path).unwrap().modified().unwrap(),
            modified
        );
    }
    #[test]
    fn invalid_or_missing_video_fails_without_generation() {
        let f = Fixture::new();
        assert!(extract_last(&f.0, &f.clip().with_file_name("missing.mp4"), &f.1).is_err());
        std::fs::write(f.clip(), b"not-a-video").unwrap();
        assert!(extract_last(&f.0, &f.clip(), &f.1).is_err());
    }
    #[test]
    fn no_shell_or_remote_protocol() {
        let f = Fixture::new();
        assert!(extract_last(
            &f.0,
            std::path::Path::new("https://example.test/a.mp4"),
            &f.1
        )
        .is_err());
        assert!(extract_last(&f.0, &f.clip(), &f.1.with_file_name("cmd.exe")).is_err());
        std::fs::write(f.0.join("video-frame-extractor.json"), b"{}").unwrap();
        assert!(extract_last(&f.0, &f.clip(), &f.1).is_err());
    }
    #[test]
    fn timeout_kills_process_and_releases_slot() {
        let f = Fixture::new();
        assert_eq!(
            extract_with_timeout(&f.0, &f.clip(), &f.1, std::time::Duration::ZERO).unwrap_err(),
            "frame_extraction_timeout"
        );
        assert!(extract_last(&f.0, &f.clip(), &f.1).is_ok());
    }
    #[test]
    fn cleanup_waits_for_media_lease() {
        let f = Fixture::new();
        let guard = MediaLease::acquire(&f.0, &f.clip()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert_eq!(super::super::video_store::cleanup(&f.0, 1), 0);
        assert!(f.clip().exists());
        drop(guard);
        assert_eq!(super::super::video_store::cleanup(&f.0, 1), 1);
    }
    #[test]
    fn oversized_png_and_extractor_config_are_rejected_before_read() {
        let f = Fixture::new();
        let path = f.0.join("oversized.png");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FRAME + 1).unwrap();
        drop(file);
        assert!(read_small(&path, MAX_FRAME).is_err());
        assert!(png(&path).is_err());
        let config = f.0.join("video-frame-extractor.json");
        let file = File::create(config).unwrap();
        file.set_len(8193).unwrap();
        drop(file);
        assert_eq!(
            configured_extractor(&f.0).unwrap_err(),
            "frame_extractor_unconfigured"
        );
    }
    #[test]
    fn extraction_has_two_process_slots_and_eight_bounded_waiters() {
        let l = std::sync::Arc::new(Limits {
            counts: Mutex::new(Counts::default()),
            ready: Condvar::new(),
        });
        let a = Slot::on(l.clone()).unwrap();
        let b = Slot::on(l.clone()).unwrap();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let l = l.clone();
            joins.push(std::thread::spawn(move || {
                let guard = Slot::on(l.clone()).unwrap();
                assert!(l.counts.lock().unwrap().active <= 2);
                drop(guard);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if l.counts.lock().unwrap().queued == 8 {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(Slot::on(l.clone()),Err(e)if e=="frame_extractor_busy"));
        drop(a);
        drop(b);
        for join in joins {
            join.join().unwrap();
        }
        let c = l.counts.lock().unwrap();
        assert_eq!((c.active, c.queued), (0, 0));
    }
}

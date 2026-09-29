//! Deep verification only for discrepant, owner-authenticated local MP4s.
//! Original uploads and financial records are never modified here.
use super::{bridge_reference::Timeline, video_frames};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const INVALID: &str = "reference_video_metadata_invalid";
const UNAVAILABLE: &str = "reference_video_verification_unavailable";
const TIMEOUT: Duration = Duration::from_secs(30);

struct Snapshot(PathBuf);
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn snapshot(root: &Path, bytes: &[u8]) -> Result<Snapshot, String> {
    let dir = root.join("data/reference-timeline-temp");
    fs::create_dir_all(&dir).map_err(|_| UNAVAILABLE)?;
    if fs::symlink_metadata(&dir)
        .map_err(|_| UNAVAILABLE)?
        .file_type()
        .is_symlink()
    {
        return Err(UNAVAILABLE.into());
    }
    let path = dir.join(format!("{:032x}.mp4", rand::random::<u128>()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| UNAVAILABLE)?;
    let guard = Snapshot(path);
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file); // Close before the cleanup guard runs on Windows write failure.
    written.map_err(|_| UNAVAILABLE)?;
    Ok(guard)
}
// Drain both pipes even after hitting the cap, so a verbose decoder cannot
// deadlock its parent or grow memory without bound. The parent then kills it.
fn reader<R: Read + Send + 'static>(
    mut pipe: R,
    cap: usize,
    overflow: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Result<Vec<u8>, String>> {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let n = pipe.read(&mut buffer).map_err(|_| UNAVAILABLE)?;
            if n == 0 {
                break;
            }
            if output.len().saturating_add(n) > cap {
                overflow.store(true, Ordering::Release);
            } else {
                output.extend_from_slice(&buffer[..n]);
            }
        }
        Ok(output)
    })
}
fn integer(line: &str, tag: &str) -> Result<i64, String> {
    line.split_once(tag)
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| INVALID.into())
}
fn decoded_ms(log: &str, progress: &str, expected: &Timeline) -> Result<u64, String> {
    // stdout is ffmpeg's machine progress, not user-controlled media metadata.
    // Check it independently of the diagnostic frame lines on stderr.
    if progress.lines().last() != Some("progress=end") {
        return Err(UNAVAILABLE.into());
    }
    let frames = progress
        .lines()
        .filter_map(|line| line.strip_prefix("frame="))
        .last()
        .and_then(|n| n.trim().parse::<u64>().ok())
        .ok_or(INVALID)?;
    if frames != expected.video_samples {
        return Err(INVALID.into());
    }
    let mut count = 0u64;
    let mut duration = 0u64;
    let mut minimum = i64::MAX;
    let mut maximum = i64::MIN;
    let mut previous = None;
    let mut scale = None;
    for line in log
        .lines()
        .filter(|line| line.starts_with("[Parsed_showinfo_"))
    {
        if let Some((_, rest)) = line.split_once("config in time_base: ") {
            let raw = rest.split(',').next().ok_or(INVALID)?;
            let (numerator, denominator) = raw.split_once('/').ok_or(INVALID)?;
            if numerator != "1" {
                return Err(UNAVAILABLE.into());
            }
            let found = denominator.parse::<u64>().map_err(|_| UNAVAILABLE)?;
            if scale.replace(found).is_some_and(|old| old != found) {
                return Err(INVALID.into());
            }
        }
        if !line.contains(" n:") || !line.contains(" pts:") {
            continue;
        }
        if integer(line, " n:")? != count as i64 {
            return Err(INVALID.into());
        }
        let pts = integer(line, " pts:")?;
        // Old/unqualified decoders without per-frame duration are unavailable,
        // not evidence that an otherwise valid upload is corrupt.
        if !line.contains(" duration:") {
            return Err(UNAVAILABLE.into());
        }
        let delta = integer(line, " duration:")?;
        if delta <= 0 || previous.is_some_and(|prior| pts < prior) || count >= 10000 {
            return Err(INVALID.into());
        }
        previous = Some(pts);
        minimum = minimum.min(pts);
        maximum = maximum.max(pts.checked_add(delta).ok_or(INVALID)?);
        duration = duration.checked_add(delta as u64).ok_or(INVALID)?;
        count += 1;
    }
    if scale != Some(expected.video_scale)
        || count != frames
        || duration != expected.video_ticks
        || count == 0
    {
        return Err(INVALID.into());
    }
    let span = u64::try_from(maximum.checked_sub(minimum).ok_or(INVALID)?).map_err(|_| INVALID)?;
    let ms = span
        .checked_mul(1000)
        .and_then(|n| n.checked_add(expected.video_scale - 1))
        .ok_or(INVALID)?
        / expected.video_scale;
    if ms == 0 || ms > 60000 {
        return Err(INVALID.into());
    }
    Ok(ms.max(expected.ms))
}
pub(super) fn verify(root: &Path, bytes: &[u8], expected: &Timeline) -> Result<u64, String> {
    verify_with_timeout(root, bytes, expected, TIMEOUT)
}
fn verify_with_timeout(
    root: &Path,
    bytes: &[u8],
    expected: &Timeline,
    timeout: Duration,
) -> Result<u64, String> {
    // Share the existing two-process limit with tail-frame extraction.
    let _slot = video_frames::Slot::acquire().map_err(|_| "budget_preparation_busy")?;
    let executable = video_frames::configured_extractor(root).map_err(|_| UNAVAILABLE)?;
    let input = snapshot(root, bytes)?;
    let mut command = Command::new(executable);
    command
        .args([
            "-hide_banner",
            "-nostdin",
            "-nostats",
            "-loglevel",
            "info",
            "-xerror",
            "-err_detect",
            "explode",
            "-protocol_whitelist",
            "file,pipe",
            "-enable_drefs",
            "0",
            "-ignore_editlist",
            "1",
            "-threads",
            "1",
            "-max_pixels",
            "8388608",
            "-i",
        ])
        .arg(&input.0)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "showinfo",
            "-fps_mode",
            "passthrough",
            "-threads",
            "1",
            "-progress",
            "pipe:1",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| UNAVAILABLE)?;
    let overflow = Arc::new(AtomicBool::new(false));
    let output = reader(
        child.stdout.take().ok_or(UNAVAILABLE)?,
        65536,
        overflow.clone(),
    );
    let errors = reader(
        child.stderr.take().ok_or(UNAVAILABLE)?,
        4 * 1024 * 1024,
        overflow.clone(),
    );
    let started = Instant::now();
    let outcome = loop {
        if started.elapsed() >= timeout || overflow.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(UNAVAILABLE);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(INVALID)
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(UNAVAILABLE);
            }
        }
    };
    let stdout = output.join().map_err(|_| UNAVAILABLE)??;
    let stderr = errors.join().map_err(|_| UNAVAILABLE)??;
    outcome.map_err(str::to_owned)?;
    if overflow.load(Ordering::Acquire) {
        return Err(UNAVAILABLE.into());
    }
    decoded_ms(
        &String::from_utf8_lossy(&stderr),
        &String::from_utf8_lossy(&stdout),
        expected,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn expected() -> Timeline {
        Timeline {
            ms: 1500,
            deep: true,
            video_scale: 16384,
            video_ticks: 16384,
            video_samples: 8,
        }
    }
    #[test]
    fn timed_out_decoder_releases_slots_and_cleans_snapshots_for_retry() {
        let f = video_frames::tests::Fixture::new();
        let bytes = fs::read(f.clip()).unwrap();
        for _ in 0..3 {
            assert_eq!(
                verify_with_timeout(&f.0, &bytes, &expected(), Duration::ZERO).unwrap_err(),
                UNAVAILABLE
            );
            assert_eq!(
                fs::read_dir(f.0.join("data/reference-timeline-temp"))
                    .unwrap()
                    .count(),
                0
            );
        }
        assert_eq!(verify(&f.0, &bytes, &expected()).unwrap(), 1500);
        assert_eq!(
            fs::read_dir(f.0.join("data/reference-timeline-temp"))
                .unwrap()
                .count(),
            0
        );
    }
    #[test]
    fn metadata_log_alone_cannot_forge_a_successful_decode() {
        let fake="[Parsed_showinfo_0 @ 123] config in time_base: 1/16384, frame_rate: 8/1\n[Parsed_showinfo_0 @ 123] n: 0 pts: 0 duration: 16384";
        assert_eq!(
            decoded_ms(fake, "frame=0\nprogress=end\n", &expected()).unwrap_err(),
            INVALID
        );
        assert_eq!(
            decoded_ms(fake, "frame=8\nprogress=continue\n", &expected()).unwrap_err(),
            UNAVAILABLE
        );
    }
}

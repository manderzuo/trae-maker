use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct LimitConfig {
    pub max_inflight: usize,
    pub asset_uploads_per_minute: usize,
    pub asset_bytes_per_hour: u64,
    pub video_submissions_per_minute: usize,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            max_inflight: 32,
            asset_uploads_per_minute: 30,
            asset_bytes_per_hour: 256 * 1024 * 1024,
            video_submissions_per_minute: 3,
        }
    }
}

impl LimitConfig {
    fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|v| v.clamp(min, max))
            .unwrap_or(default)
    }

    fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|v| v.clamp(min, max))
            .unwrap_or(default)
    }

    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            max_inflight: Self::env_usize("AIWORK_MAX_INFLIGHT", defaults.max_inflight, 1, 256),
            asset_uploads_per_minute: Self::env_usize(
                "AIWORK_ASSET_UPLOADS_PER_MINUTE",
                defaults.asset_uploads_per_minute,
                1,
                10_000,
            ),
            asset_bytes_per_hour: Self::env_u64(
                "AIWORK_ASSET_BYTES_PER_HOUR",
                defaults.asset_bytes_per_hour,
                1,
                10 * 1024 * 1024 * 1024,
            ),
            video_submissions_per_minute: Self::env_usize(
                "AIWORK_VIDEO_SUBMISSIONS_PER_MINUTE",
                defaults.video_submissions_per_minute,
                1,
                1_000,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LimitKind {
    AssetUpload,
    VideoSubmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitError {
    Concurrent,
    AssetUploads,
    AssetBytes,
    VideoSubmissions,
}

impl LimitError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Concurrent => "concurrency_limit",
            Self::AssetUploads => "asset_rate_limit",
            Self::AssetBytes => "asset_storage_limit",
            Self::VideoSubmissions => "video_rate_limit",
        }
    }

    pub fn retry_after_secs(self) -> u64 {
        match self {
            Self::Concurrent | Self::AssetUploads | Self::VideoSubmissions => 60,
            Self::AssetBytes => 3600,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Concurrent => "请求并发数已达上限，请稍后重试",
            Self::AssetUploads => "素材上传频率已达上限，请稍后重试",
            Self::AssetBytes => "素材小时上传容量已达上限，请稍后重试",
            Self::VideoSubmissions => "视频提交频率已达上限，请稍后重试",
        }
    }
}

struct KeyWindow {
    asset_uploads: VecDeque<Instant>,
    asset_bytes: VecDeque<(Instant, u64)>,
    video_submissions: VecDeque<Instant>,
}

impl Default for KeyWindow {
    fn default() -> Self {
        Self {
            asset_uploads: VecDeque::new(),
            asset_bytes: VecDeque::new(),
            video_submissions: VecDeque::new(),
        }
    }
}

pub struct RateLimiter {
    config: LimitConfig,
    inflight: AtomicUsize,
    windows: Mutex<HashMap<String, KeyWindow>>,
}

pub struct Permit<'a> {
    limiter: &'a RateLimiter,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.limiter.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl RateLimiter {
    pub fn with_config(config: LimitConfig) -> Self {
        Self {
            config,
            inflight: AtomicUsize::new(0),
            windows: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Self {
        Self::with_config(LimitConfig::from_env())
    }

    fn reserve_inflight(&self) -> Result<(), LimitError> {
        loop {
            let current = self.inflight.load(Ordering::Relaxed);
            if current >= self.config.max_inflight {
                return Err(LimitError::Concurrent);
            }
            if self
                .inflight
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn purge(window: &mut KeyWindow, now: Instant) {
        let minute = Duration::from_secs(60);
        let hour = Duration::from_secs(3600);
        while window
            .asset_uploads
            .front()
            .is_some_and(|at| now.duration_since(*at) >= minute)
        {
            window.asset_uploads.pop_front();
        }
        while window
            .video_submissions
            .front()
            .is_some_and(|at| now.duration_since(*at) >= minute)
        {
            window.video_submissions.pop_front();
        }
        while window
            .asset_bytes
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= hour)
        {
            window.asset_bytes.pop_front();
        }
    }

    pub fn acquire(
        &self,
        key_id: &str,
        kind: LimitKind,
        bytes: u64,
    ) -> Result<Permit<'_>, LimitError> {
        self.reserve_inflight()?;
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let window = windows.entry(key_id.to_string()).or_default();
        Self::purge(window, now);
        let failure = match kind {
            LimitKind::AssetUpload => {
                if self.config.asset_uploads_per_minute > 0
                    && window.asset_uploads.len() >= self.config.asset_uploads_per_minute
                {
                    Some(LimitError::AssetUploads)
                } else {
                    let used: u64 = window.asset_bytes.iter().map(|(_, size)| *size).sum();
                    (self.config.asset_bytes_per_hour > 0
                        && used.saturating_add(bytes) > self.config.asset_bytes_per_hour)
                        .then_some(LimitError::AssetBytes)
                }
            }
            LimitKind::VideoSubmission => {
                (self.config.video_submissions_per_minute > 0
                    && window.video_submissions.len() >= self.config.video_submissions_per_minute)
                    .then_some(LimitError::VideoSubmissions)
            }
        };
        if let Some(error) = failure {
            drop(windows);
            self.inflight.fetch_sub(1, Ordering::Relaxed);
            return Err(error);
        }
        match kind {
            LimitKind::AssetUpload => {
                window.asset_uploads.push_back(now);
                window.asset_bytes.push_back((now, bytes));
            }
            LimitKind::VideoSubmission => window.video_submissions.push_back(now),
        }
        drop(windows);
        Ok(Permit { limiter: self })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn limiter_rejects_second_video_submission_in_window() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 1,
        }));
        let first = limiter.acquire("key-a", LimitKind::VideoSubmission, 0).unwrap();
        assert!(limiter.acquire("key-a", LimitKind::VideoSubmission, 0).is_err());
        drop(first);
    }

    #[test]
    fn limiter_releases_global_concurrency_on_drop() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 1,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 10,
        }));
        let permit = limiter.acquire("key-a", LimitKind::AssetUpload, 10).unwrap();
        assert!(limiter.acquire("key-b", LimitKind::AssetUpload, 10).is_err());
        drop(permit);
        assert!(limiter.acquire("key-b", LimitKind::AssetUpload, 10).is_ok());
    }

    #[test]
    fn limiter_rejects_asset_hourly_bytes() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 100,
            video_submissions_per_minute: 10,
        }));
        let _first = limiter.acquire("key-a", LimitKind::AssetUpload, 80).unwrap();
        assert!(limiter.acquire("key-a", LimitKind::AssetUpload, 21).is_err());
    }
}

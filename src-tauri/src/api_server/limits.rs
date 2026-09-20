use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::api_keys::KeyLimits;

const MAX_GLOBAL_INFLIGHT: usize = 256;
const MAX_GLOBAL_VIDEO_JOBS: usize = 256;
const MAX_GLOBAL_ASSET_UPLOADS_PER_MINUTE: usize = 10_000;
const MAX_GLOBAL_ASSET_BYTES_PER_HOUR: u64 = 10 * 1024 * 1024 * 1024;
const MAX_GLOBAL_VIDEO_SUBMISSIONS_PER_MINUTE: usize = 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct LimitConfig {
    pub max_inflight: usize,
    pub max_video_jobs: usize,
    pub asset_uploads_per_minute: usize,
    pub asset_bytes_per_hour: u64,
    pub video_submissions_per_minute: usize,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            max_inflight: 32,
            max_video_jobs: 32,
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

    /// Clamp persisted values to the same safe ranges used by environment
    /// overrides. Global zero values are not treated as unlimited; only the
    /// daily Key counters use zero for that meaning.
    pub fn normalized(mut self) -> Self {
        self.max_inflight = self.max_inflight.clamp(1, MAX_GLOBAL_INFLIGHT);
        self.max_video_jobs = self.max_video_jobs.clamp(1, MAX_GLOBAL_VIDEO_JOBS);
        self.asset_uploads_per_minute = self
            .asset_uploads_per_minute
            .clamp(1, MAX_GLOBAL_ASSET_UPLOADS_PER_MINUTE);
        self.asset_bytes_per_hour = self
            .asset_bytes_per_hour
            .clamp(1, MAX_GLOBAL_ASSET_BYTES_PER_HOUR);
        self.video_submissions_per_minute = self
            .video_submissions_per_minute
            .clamp(1, MAX_GLOBAL_VIDEO_SUBMISSIONS_PER_MINUTE);
        self
    }

    pub fn normalize(&mut self) {
        *self = self.normalized();
    }

    /// Apply the four legacy deployment overrides to persisted defaults.
    /// Invalid values fall back to the persisted value; valid values are
    /// clamped to the safe range.
    pub(crate) fn with_env_overrides(mut self) -> Self {
        let persisted = self.normalized();
        self.max_inflight = Self::env_usize(
            "AIWORK_MAX_INFLIGHT",
            persisted.max_inflight,
            1,
            MAX_GLOBAL_INFLIGHT,
        );
        self.asset_uploads_per_minute = Self::env_usize(
            "AIWORK_ASSET_UPLOADS_PER_MINUTE",
            persisted.asset_uploads_per_minute,
            1,
            MAX_GLOBAL_ASSET_UPLOADS_PER_MINUTE,
        );
        self.asset_bytes_per_hour = Self::env_u64(
            "AIWORK_ASSET_BYTES_PER_HOUR",
            persisted.asset_bytes_per_hour,
            1,
            MAX_GLOBAL_ASSET_BYTES_PER_HOUR,
        );
        self.video_submissions_per_minute = Self::env_usize(
            "AIWORK_VIDEO_SUBMISSIONS_PER_MINUTE",
            persisted.video_submissions_per_minute,
            1,
            MAX_GLOBAL_VIDEO_SUBMISSIONS_PER_MINUTE,
        );
        self.max_video_jobs = persisted.max_video_jobs;
        self.normalized()
    }

    pub fn from_env() -> Self {
        Self::default().with_env_overrides()
    }
}

/// Return a Key's effective windows without ever allowing a Key override to
/// exceed the configured global cap. This inherent implementation lives next
/// to the limiter so the Task 1 data model remains unchanged.
impl KeyLimits {
    pub fn effective(&self, global: &LimitConfig) -> LimitConfig {
        let key = self.clone().normalized();
        let global = global.normalized();
        LimitConfig {
            max_inflight: key
                .max_inflight
                .unwrap_or(global.max_inflight)
                .min(global.max_inflight),
            max_video_jobs: key
                .max_video_jobs
                .unwrap_or(global.max_video_jobs)
                .min(global.max_video_jobs),
            asset_uploads_per_minute: key
                .asset_uploads_per_minute
                .unwrap_or(global.asset_uploads_per_minute)
                .min(global.asset_uploads_per_minute),
            asset_bytes_per_hour: key
                .asset_bytes_per_hour
                .unwrap_or(global.asset_bytes_per_hour)
                .min(global.asset_bytes_per_hour),
            video_submissions_per_minute: key
                .video_submissions_per_minute
                .unwrap_or(global.video_submissions_per_minute)
                .min(global.video_submissions_per_minute),
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
    inflight: usize,
    video_jobs: usize,
    asset_uploads: VecDeque<Instant>,
    asset_bytes: VecDeque<(Instant, u64)>,
    video_submissions: VecDeque<Instant>,
}

impl Default for KeyWindow {
    fn default() -> Self {
        Self {
            inflight: 0,
            video_jobs: 0,
            asset_uploads: VecDeque::new(),
            asset_bytes: VecDeque::new(),
            video_submissions: VecDeque::new(),
        }
    }
}

struct RateLimiterInner {
    config: LimitConfig,
    inflight: AtomicUsize,
    video_jobs: AtomicUsize,
    windows: Mutex<HashMap<String, KeyWindow>>,
}

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<RateLimiterInner>,
}

enum PermitKind {
    Request { key_id: String },
    VideoJob { key_id: String },
}

pub struct Permit {
    inner: Arc<RateLimiterInner>,
    kind: PermitKind,
}

impl Drop for Permit {
    fn drop(&mut self) {
        match &self.kind {
            PermitKind::Request { key_id } => release_request(&self.inner, key_id),
            PermitKind::VideoJob { key_id } => release_video_job(&self.inner, key_id),
        }
    }
}

impl RateLimiter {
    pub fn with_config(config: LimitConfig) -> Self {
        Self {
            inner: Arc::new(RateLimiterInner {
                config: config.normalized(),
                inflight: AtomicUsize::new(0),
                video_jobs: AtomicUsize::new(0),
                windows: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn from_env() -> Self {
        Self::with_config(LimitConfig::from_env())
    }

    fn reserve_global(counter: &AtomicUsize, max: usize) -> Result<(), LimitError> {
        loop {
            let current = counter.load(Ordering::Relaxed);
            if current >= max {
                return Err(LimitError::Concurrent);
            }
            if counter
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn reserve_request(&self, key_id: &str, effective: LimitConfig) -> Result<(), LimitError> {
        let mut windows = self
            .inner
            .windows
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let window = windows.entry(key_id.to_string()).or_default();
        if window.inflight >= effective.max_inflight {
            return Err(LimitError::Concurrent);
        }
        if let Err(error) =
            Self::reserve_global(&self.inner.inflight, self.inner.config.max_inflight)
        {
            return Err(error);
        }
        window.inflight += 1;
        Ok(())
    }

    fn reserve_video_job(&self, key_id: &str, effective: LimitConfig) -> Result<(), LimitError> {
        let mut windows = self
            .inner
            .windows
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let window = windows.entry(key_id.to_string()).or_default();
        if window.video_jobs >= effective.max_video_jobs {
            return Err(LimitError::Concurrent);
        }
        Self::reserve_global(&self.inner.video_jobs, self.inner.config.max_video_jobs)?;
        window.video_jobs += 1;
        Ok(())
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

    pub fn acquire_request(
        &self,
        key_id: &str,
        key_limits: &KeyLimits,
    ) -> Result<Permit, LimitError> {
        let effective = key_limits.effective(&self.inner.config);
        self.reserve_request(key_id, effective)?;
        Ok(Permit {
            inner: self.inner.clone(),
            kind: PermitKind::Request {
                key_id: key_id.to_string(),
            },
        })
    }

    pub fn acquire(
        &self,
        key_id: &str,
        kind: LimitKind,
        bytes: u64,
        key_limits: &KeyLimits,
    ) -> Result<Permit, LimitError> {
        let effective = key_limits.effective(&self.inner.config);
        self.reserve_request(key_id, effective)?;
        let now = Instant::now();
        let mut windows = self
            .inner
            .windows
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let window = windows.entry(key_id.to_string()).or_default();
        Self::purge(window, now);
        let failure = match kind {
            LimitKind::AssetUpload => {
                if effective.asset_uploads_per_minute > 0
                    && window.asset_uploads.len() >= effective.asset_uploads_per_minute
                {
                    Some(LimitError::AssetUploads)
                } else {
                    let used: u64 = window.asset_bytes.iter().map(|(_, size)| *size).sum();
                    (effective.asset_bytes_per_hour > 0
                        && used.saturating_add(bytes) > effective.asset_bytes_per_hour)
                        .then_some(LimitError::AssetBytes)
                }
            }
            LimitKind::VideoSubmission => {
                (effective.video_submissions_per_minute > 0
                    && window.video_submissions.len() >= effective.video_submissions_per_minute)
                    .then_some(LimitError::VideoSubmissions)
            }
        };
        if let Some(error) = failure {
            drop(windows);
            release_request(&self.inner, key_id);
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
        Ok(Permit {
            inner: self.inner.clone(),
            kind: PermitKind::Request {
                key_id: key_id.to_string(),
            },
        })
    }

    /// Compatibility adapter for callers that have not yet received the
    /// authenticated Key policy snapshot. New callers should use `acquire`
    /// with explicit KeyLimits.
    pub fn acquire_legacy(
        &self,
        key_id: &str,
        kind: LimitKind,
        bytes: u64,
    ) -> Result<Permit, LimitError> {
        self.acquire(key_id, kind, bytes, &KeyLimits::default())
    }

    pub fn acquire_video_job(
        &self,
        key_id: &str,
        key_limits: &KeyLimits,
    ) -> Result<Permit, LimitError> {
        let effective = key_limits.effective(&self.inner.config);
        self.reserve_video_job(key_id, effective)?;
        Ok(Permit {
            inner: self.inner.clone(),
            kind: PermitKind::VideoJob {
                key_id: key_id.to_string(),
            },
        })
    }
}

fn release_request(inner: &RateLimiterInner, key_id: &str) {
    inner.inflight.fetch_sub(1, Ordering::Relaxed);
    let mut windows = inner.windows.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(window) = windows.get_mut(key_id) {
        window.inflight = window.inflight.saturating_sub(1);
        remove_idle_window(&mut windows, key_id);
    }
}

fn release_video_job(inner: &RateLimiterInner, key_id: &str) {
    inner.video_jobs.fetch_sub(1, Ordering::Relaxed);
    let mut windows = inner.windows.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(window) = windows.get_mut(key_id) {
        window.video_jobs = window.video_jobs.saturating_sub(1);
        remove_idle_window(&mut windows, key_id);
    }
}

fn remove_idle_window(windows: &mut HashMap<String, KeyWindow>, key_id: &str) {
    let idle = windows.get(key_id).is_some_and(|window| {
        window.inflight == 0
            && window.video_jobs == 0
            && window.asset_uploads.is_empty()
            && window.asset_bytes.is_empty()
            && window.video_submissions.is_empty()
    });
    if idle {
        windows.remove(key_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::api_keys::KeyLimits;
    use std::sync::Arc;

    fn key_limits() -> KeyLimits {
        KeyLimits::default()
    }

    #[test]
    fn limiter_rejects_second_video_submission_in_window() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            max_video_jobs: 4,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 1,
        }));
        let first = limiter
            .acquire("key-a", LimitKind::VideoSubmission, 0, &key_limits())
            .unwrap();
        assert!(limiter
            .acquire("key-a", LimitKind::VideoSubmission, 0, &key_limits())
            .is_err());
        drop(first);
    }

    #[test]
    fn request_concurrency_isolated_by_key_and_capped_by_global_limit() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 2,
            max_video_jobs: 2,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 10,
        }));
        let key_a = KeyLimits {
            max_inflight: Some(1),
            ..key_limits()
        };
        let key_b = key_limits();
        let a = limiter.acquire_request("key-a", &key_a).unwrap();
        assert!(matches!(
            limiter.acquire_request("key-a", &key_a),
            Err(LimitError::Concurrent)
        ));
        let b = limiter.acquire_request("key-b", &key_b).unwrap();
        assert!(matches!(
            limiter.acquire_request("key-b", &key_b),
            Err(LimitError::Concurrent)
        ));
        drop(a);
        drop(b);
        assert!(limiter.acquire_request("key-b", &key_b).is_ok());
    }

    #[test]
    fn key_override_can_only_reduce_the_effective_global_limit() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 2,
            max_video_jobs: 2,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 10,
        }));
        let key = KeyLimits {
            max_inflight: Some(99),
            ..key_limits()
        };
        let first = limiter.acquire_request("key-a", &key).unwrap();
        let second = limiter.acquire_request("key-a", &key).unwrap();
        assert!(matches!(
            limiter.acquire_request("key-a", &key),
            Err(LimitError::Concurrent)
        ));
        drop(first);
        drop(second);

        let reduced = KeyLimits {
            max_inflight: Some(1),
            ..key_limits()
        };
        let permit = limiter.acquire_request("key-b", &reduced).unwrap();
        assert!(matches!(
            limiter.acquire_request("key-b", &reduced),
            Err(LimitError::Concurrent)
        ));
        drop(permit);
    }

    #[test]
    fn video_job_permit_is_held_until_drop() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            max_video_jobs: 1,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 10,
        }));
        let first = limiter.acquire_video_job("key-a", &key_limits()).unwrap();
        assert!(matches!(
            limiter.acquire_video_job("key-a", &key_limits()),
            Err(LimitError::Concurrent)
        ));
        drop(first);
        assert!(limiter.acquire_video_job("key-a", &key_limits()).is_ok());
    }

    #[test]
    fn per_key_video_and_asset_windows_use_overrides() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            max_video_jobs: 4,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1_000,
            video_submissions_per_minute: 10,
        }));
        let limits = KeyLimits {
            asset_uploads_per_minute: Some(1),
            asset_bytes_per_hour: Some(8),
            video_submissions_per_minute: Some(1),
            ..key_limits()
        };

        let video = limiter
            .acquire("key-a", LimitKind::VideoSubmission, 0, &limits)
            .unwrap();
        drop(video);
        assert!(matches!(
            limiter.acquire("key-a", LimitKind::VideoSubmission, 0, &limits),
            Err(LimitError::VideoSubmissions)
        ));

        let asset = limiter
            .acquire("key-b", LimitKind::AssetUpload, 8, &limits)
            .unwrap();
        drop(asset);
        assert!(matches!(
            limiter.acquire("key-b", LimitKind::AssetUpload, 1, &limits),
            Err(LimitError::AssetUploads)
        ));
    }

    #[test]
    fn limiter_releases_global_concurrency_on_drop() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 1,
            max_video_jobs: 1,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 1024,
            video_submissions_per_minute: 10,
        }));
        let permit = limiter
            .acquire("key-a", LimitKind::AssetUpload, 10, &key_limits())
            .unwrap();
        assert!(limiter
            .acquire("key-b", LimitKind::AssetUpload, 10, &key_limits())
            .is_err());
        drop(permit);
        assert!(limiter
            .acquire("key-b", LimitKind::AssetUpload, 10, &key_limits())
            .is_ok());
    }

    #[test]
    fn limiter_rejects_asset_hourly_bytes() {
        let limiter = Arc::new(RateLimiter::with_config(LimitConfig {
            max_inflight: 4,
            max_video_jobs: 4,
            asset_uploads_per_minute: 10,
            asset_bytes_per_hour: 100,
            video_submissions_per_minute: 10,
        }));
        let _first = limiter
            .acquire("key-a", LimitKind::AssetUpload, 80, &key_limits())
            .unwrap();
        assert!(limiter
            .acquire("key-a", LimitKind::AssetUpload, 21, &key_limits())
            .is_err());
    }
}

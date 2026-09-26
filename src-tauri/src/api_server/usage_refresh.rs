//! Bounded background refresh of usage for uniquely linked Core sessions.
//! Upstream reads run on a fixed worker pool and never hold the queue/cache lock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::ApiSharedState;

const WORKER_COUNT: usize = 4;
const RETRY_SECONDS: [u64; 5] = [2, 5, 15, 30, 60];
const COMPENSATION_SECONDS: u64 = 300;
const STOP_POLL: Duration = Duration::from_millis(250);

struct AccountSchedule {
    attempt_at_ms: i64,
    generation: u64,
    in_flight: bool,
    retries: usize,
    due_at: Instant,
    seen_request_ids: HashSet<String>,
}

#[derive(Default)]
struct QueueState {
    accounts: BTreeMap<String, AccountSchedule>,
}

struct RefreshQueue {
    state: Mutex<QueueState>,
    changed: Condvar,
    stop: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(super) struct WorkItem {
    pub(super) account_ref: String,
    attempt_at_ms: i64,
    generation: u64,
}

type PendingLoader = dyn Fn() -> Result<Vec<(String, i64)>, String> + Send + Sync;
type RefreshHandler = dyn Fn(&WorkItem) -> bool + Send + Sync;
type WorkerSpawner = dyn Fn(
        usize,
        Arc<RefreshQueue>,
        Arc<RefreshHandler>,
    ) -> std::io::Result<JoinHandle<()>>
    + Send
    + Sync;

struct SchedulerRequest {
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: Arc<PendingLoader>,
    refresh: Arc<RefreshHandler>,
    worker_spawner: Arc<WorkerSpawner>,
}

struct SchedulerRegistration {
    queue: Weak<RefreshQueue>,
    handoff: Option<SchedulerRequest>,
}

fn schedulers() -> &'static Mutex<HashMap<PathBuf, SchedulerRegistration>> {
    static SCHEDULERS: OnceLock<Mutex<HashMap<PathBuf, SchedulerRegistration>>> = OnceLock::new();
    SCHEDULERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn data_dir_key(data_dir: &Path) -> PathBuf {
    std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf())
}

impl RefreshQueue {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
            stop,
        }
    }

    fn enqueue(&self, account_ref: String, attempt_at_ms: i64, request_id: Option<&str>) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_new = !state.accounts.contains_key(&account_ref);
        let entry = state.accounts.entry(account_ref).or_insert_with(|| AccountSchedule {
            attempt_at_ms,
            generation: 1,
            in_flight: false,
            retries: 0,
            due_at: now,
            seen_request_ids: HashSet::new(),
        });
        entry.attempt_at_ms = entry.attempt_at_ms.min(attempt_at_ms);
        if let Some(request_id) = request_id {
            if entry.seen_request_ids.insert(request_id.to_string()) {
                if !is_new {
                    entry.generation = entry.generation.saturating_add(1);
                    if !entry.in_flight {
                        entry.retries = 0;
                        entry.due_at = now;
                    }
                }
            }
        }
        drop(state);
        self.changed.notify_all();
    }

    fn claim_ready(&self, now: Instant) -> Option<WorkItem> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let account_ref = state.accounts.iter()
            .filter(|(_, entry)| !entry.in_flight && entry.due_at <= now)
            .min_by_key(|(_, entry)| entry.due_at)
            .map(|(account_ref, _)| account_ref.clone())?;
        let entry = state.accounts.get_mut(&account_ref)?;
        entry.in_flight = true;
        Some(WorkItem {
            account_ref,
            attempt_at_ms: entry.attempt_at_ms,
            generation: entry.generation,
        })
    }

    fn wait_for_work(&self) -> Option<WorkItem> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if self.stop.load(Ordering::Acquire) {
                return None;
            }
            let now = Instant::now();
            if let Some(account_ref) = state.accounts.iter()
                .filter(|(_, entry)| !entry.in_flight && entry.due_at <= now)
                .min_by_key(|(_, entry)| entry.due_at)
                .map(|(account_ref, _)| account_ref.clone())
            {
                let entry = state.accounts.get_mut(&account_ref)?;
                entry.in_flight = true;
                return Some(WorkItem {
                    account_ref,
                    attempt_at_ms: entry.attempt_at_ms,
                    generation: entry.generation,
                });
            }
            let until_next = state.accounts.values()
                .filter(|entry| !entry.in_flight)
                .map(|entry| entry.due_at.saturating_duration_since(now))
                .min()
                .unwrap_or(STOP_POLL)
                .min(STOP_POLL);
            let (next_state, _) = self.changed.wait_timeout(state, until_next)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next_state;
        }
    }

    fn finish(&self, item: &WorkItem, still_pending: bool) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = state.accounts.get_mut(&item.account_ref) else {
            return;
        };
        entry.in_flight = false;
        if entry.generation != item.generation {
            // Collapse all arrivals during this read into one immediate follow-up.
            entry.retries = 0;
            entry.due_at = Instant::now();
        } else if !still_pending {
            state.accounts.remove(&item.account_ref);
        } else {
            let delay = RETRY_SECONDS.get(entry.retries).copied()
                .unwrap_or(COMPENSATION_SECONDS);
            entry.retries = entry.retries.saturating_add(1);
            entry.due_at = Instant::now() + Duration::from_secs(delay);
        }
        drop(state);
        self.changed.notify_all();
    }

    fn wake_all(&self) {
        self.changed.notify_all();
    }
}

/// Queue one refresh only after checking the persisted, unique request mapping.
/// This function never performs an upstream request or accepts a caller-chosen account.
pub(crate) fn request_refresh(state: Arc<ApiSharedState>, request_id: &str) -> Result<(), String> {
    let lookup = {
        let store = super::bridge_billing::BridgeBillingStore::open(&state.data_dir)?;
        store.core_session_for_request(request_id)?
    };
    let (account_ref, attempt_at_ms) = match lookup {
        super::bridge_billing::CoreBillingSessionLookup::Unique {
            account_ref, associated_at_ms, ..
        } => (account_ref, associated_at_ms),
        super::bridge_billing::CoreBillingSessionLookup::Missing
        | super::bridge_billing::CoreBillingSessionLookup::Ambiguous => {
            return Err("Core request has no unique upstream session mapping".into());
        }
    };
    let key = data_dir_key(&state.data_dir);
    let queue = schedulers().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .and_then(|registration| registration.queue.upgrade())
        .ok_or_else(|| "Core usage refresh scheduler is not running".to_string())?;
    queue.enqueue(account_ref, attempt_at_ms, Some(request_id));
    Ok(())
}

/// Start one idempotent scheduler per normalized data directory. It discovers
/// persisted pending accounts immediately and keeps a fixed pool of four workers.
pub(crate) fn start(state: Arc<ApiSharedState>, stop: Arc<AtomicBool>) {
    let pending_state = state.clone();
    let pending_loader: Arc<PendingLoader> = Arc::new(move || {
        super::bridge_billing::pending_core_session_accounts_for_poll(&pending_state.data_dir)
    });
    let refresh_state = state.clone();
    let refresh_stop = stop.clone();
    let refresh: Arc<RefreshHandler> = Arc::new(move |item| {
        refresh_pending_account(&refresh_state, item, &refresh_stop)
    });
    if let Err(error) = start_inner(state.clone(), stop, pending_loader, refresh) {
        crate::fs_utils::app_log(&state.data_dir, &format!("Core 用量后台刷新器启动失败：{error}"));
    }
}

fn start_inner(
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: Arc<PendingLoader>,
    refresh: Arc<RefreshHandler>,
) -> Result<Option<JoinHandle<()>>, String> {
    let worker_spawner: Arc<WorkerSpawner> = Arc::new(|worker_id, queue, refresh| {
        let name = format!("aiwork-core-usage-{worker_id}");
        std::thread::Builder::new().name(name).spawn(move || {
            run_worker(queue, move |item| refresh(item));
        })
    });
    start_inner_with_worker_spawner(state, stop, pending_loader, refresh, worker_spawner)
}

fn start_inner_with_worker_spawner(
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: Arc<PendingLoader>,
    refresh: Arc<RefreshHandler>,
    worker_spawner: Arc<WorkerSpawner>,
) -> Result<Option<JoinHandle<()>>, String> {
    let key = data_dir_key(&state.data_dir);
    let request = SchedulerRequest { state, stop: stop.clone(), pending_loader, refresh, worker_spawner };
    let mut running = schedulers().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    running.retain(|_, registration| {
        registration.queue.strong_count() > 0
            || registration.handoff.as_ref().is_some_and(|next| !next.stop.load(Ordering::Acquire))
    });
    if let Some(registration) = running.get_mut(&key) {
        if let Some(current) = registration.queue.upgrade() {
            if current.stop.load(Ordering::Acquire) {
                let already_handed_off = registration.handoff.as_ref()
                    .is_some_and(|next| !next.stop.load(Ordering::Acquire));
                if !already_handed_off && !request.stop.load(Ordering::Acquire) {
                    let log_dir = request.state.data_dir.clone();
                    registration.handoff = Some(request);
                    current.wake_all();
                    drop(running);
                    crate::fs_utils::app_log(
                        &log_dir,
                        "Core 用量刷新器重启已排队：等待旧读取结束",
                    );
                    return Ok(None);
                }
                return Ok(None);
            }
            return Ok(None);
        }
        running.remove(&key);
    }

    let queue = Arc::new(RefreshQueue::new(stop));
    let queue_for_thread = queue.clone();
    let key_for_thread = key.clone();
    running.insert(key.clone(), SchedulerRegistration { queue: Arc::downgrade(&queue), handoff: None });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = match std::thread::Builder::new()
        .name("aiwork-core-usage-scheduler".into())
        .spawn(move || {
            run_scheduler(key_for_thread, queue_for_thread, request, ready_tx);
        })
    {
        Ok(handle) => handle,
        Err(error) => {
            running.remove(&key);
            return Err(format!("scheduler thread creation failed: {error}"));
        }
    };
    drop(running);

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Some(handle)),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(error)
        }
        Err(_) => {
            remove_registration_if_current(&key, &queue);
            let _ = handle.join();
            Err("scheduler exited before any worker started".into())
        }
    }
}

fn remove_registration_if_current(key: &Path, queue: &Arc<RefreshQueue>) {
    let mut running = schedulers().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if running.get(key).and_then(|entry| entry.queue.upgrade())
        .is_some_and(|registered| Arc::ptr_eq(&registered, queue))
    {
        running.remove(key);
    }
}

fn take_handoff_or_remove(
    key: &Path,
    queue: &Arc<RefreshQueue>,
) -> Option<(SchedulerRequest, Arc<RefreshQueue>)> {
    let mut running = schedulers().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let is_current = running.get(key).and_then(|entry| entry.queue.upgrade())
        .is_some_and(|registered| Arc::ptr_eq(&registered, queue));
    if !is_current {
        return None;
    }
    if let Some(next) = running.get_mut(key).and_then(|entry| entry.handoff.take()) {
        let next_queue = Arc::new(RefreshQueue::new(next.stop.clone()));
        if let Some(entry) = running.get_mut(key) {
            entry.queue = Arc::downgrade(&next_queue);
        }
        Some((next, next_queue))
    } else {
        running.remove(key);
        None
    }
}

fn run_scheduler(
    key: PathBuf,
    mut queue: Arc<RefreshQueue>,
    mut request: SchedulerRequest,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let mut ready_tx = Some(ready_tx);
    loop {
        let mut workers = Vec::with_capacity(WORKER_COUNT);
        let mut worker_failures = 0;
        for worker_id in 0..WORKER_COUNT {
            match (request.worker_spawner)(worker_id, queue.clone(), request.refresh.clone()) {
                Ok(worker) => workers.push(worker),
                Err(_) => worker_failures += 1,
            }
        }
        if workers.is_empty() {
            queue.stop.store(true, Ordering::Release);
            if let Some((next, next_queue)) = take_handoff_or_remove(&key, &queue) {
                request = next;
                queue = next_queue;
                continue;
            }
            if let Some(ready) = ready_tx.take() {
                let _ = ready.send(Err("no background usage worker could be started".into()));
            } else {
                crate::fs_utils::app_log(
                    &request.state.data_dir,
                    "Core 用量轮询失败：后台 worker 无法启动",
                );
            }
            return;
        }
        if worker_failures > 0 {
            crate::fs_utils::app_log(
                &request.state.data_dir,
                "Core 用量后台 worker 部分启动失败，使用剩余 worker",
            );
        }
        if let Some(ready) = ready_tx.take() {
            let _ = ready.send(Ok(()));
        }
        if !queue.stop.load(Ordering::Acquire) {
            match (request.pending_loader)() {
                Ok(pending) => {
                    for (account_ref, attempt_at_ms) in pending {
                        queue.enqueue(account_ref, attempt_at_ms, None);
                    }
                }
                Err(_) => crate::fs_utils::app_log(
                    &request.state.data_dir,
                    "Core 用量轮询失败：待核验请求索引不可用",
                ),
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
        let Some((next, next_queue)) = take_handoff_or_remove(&key, &queue) else {
            return;
        };
        request = next;
        queue = next_queue;
    }
}

fn run_worker<F>(queue: Arc<RefreshQueue>, refresh: F)
where
    F: Fn(&WorkItem) -> bool,
{
    while !queue.stop.load(Ordering::Acquire) {
        let Some(item) = queue.wait_for_work() else {
            continue;
        };
        if queue.stop.load(Ordering::Acquire) {
            break;
        }
        let still_pending = refresh(&item);
        if queue.stop.load(Ordering::Acquire) {
            break;
        }
        queue.finish(&item, still_pending);
    }
}

fn refresh_pending_account(
    state: &ApiSharedState,
    item: &WorkItem,
    stop: &AtomicBool,
) -> bool {
    if stop.load(Ordering::Acquire) {
        return false;
    }
    let pending = match super::bridge_billing::pending_core_session_accounts_for_poll(&state.data_dir) {
        Ok(pending) => pending,
        Err(_) => {
            crate::fs_utils::app_log(&state.data_dir, "Core 用量轮询失败：待核验请求索引不可用");
            return true;
        }
    };
    if stop.load(Ordering::Acquire) {
        return false;
    }
    let Some((_, attempt_at_ms)) = pending.iter().find(|(uid, _)| uid == &item.account_ref) else {
        return false;
    };
    let account_refs = HashSet::from([item.account_ref.clone()]);
    let credentials = state.pool.usage_credentials_for(&account_refs);
    if credentials.is_empty() {
        return true;
    }
    let requested = vec![(item.account_ref.clone(), (*attempt_at_ms).min(item.attempt_at_ms))];
    if crate::commands::usage_history::refresh_pending_core_usage_with_stop(
        &state.data_dir,
        &requested,
        &credentials,
        stop,
    ).is_err() && !stop.load(Ordering::Acquire) {
        crate::fs_utils::app_log(
            &state.data_dir,
            "Core 用量轮询失败：上游只读用量查询或本地缓存不可用",
        );
    }
    if stop.load(Ordering::Acquire) {
        return false;
    }
    match super::bridge_billing::pending_core_session_accounts_for_poll(&state.data_dir) {
        Ok(pending) => pending.iter().any(|(uid, _)| uid == &item.account_ref),
        Err(_) => true,
    }
}

#[cfg(test)]
pub(super) fn start_with<L, F>(
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: L,
    refresh: F,
) -> JoinHandle<()>
where
    L: Fn() -> Result<Vec<(String, i64)>, String> + Send + Sync + 'static,
    F: Fn(&WorkItem) -> bool + Send + Sync + 'static,
{
    start_with_result(state, stop, pending_loader, refresh)
        .expect("test scheduler thread starts")
        .expect("test data directory has no active scheduler")
}

#[cfg(test)]
pub(super) fn start_with_result<L, F>(
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: L,
    refresh: F,
) -> Result<Option<JoinHandle<()>>, String>
where
    L: Fn() -> Result<Vec<(String, i64)>, String> + Send + Sync + 'static,
    F: Fn(&WorkItem) -> bool + Send + Sync + 'static,
{
    let pending_loader: Arc<PendingLoader> = Arc::new(pending_loader);
    let refresh: Arc<RefreshHandler> = Arc::new(refresh);
    start_inner(state, stop, pending_loader, refresh)
}

#[cfg(test)]
pub(super) fn start_with_no_workers<L, F>(
    state: Arc<ApiSharedState>,
    stop: Arc<AtomicBool>,
    pending_loader: L,
    refresh: F,
) -> Result<Option<JoinHandle<()>>, String>
where
    L: Fn() -> Result<Vec<(String, i64)>, String> + Send + Sync + 'static,
    F: Fn(&WorkItem) -> bool + Send + Sync + 'static,
{
    let pending_loader: Arc<PendingLoader> = Arc::new(pending_loader);
    let refresh: Arc<RefreshHandler> = Arc::new(refresh);
    let worker_spawner: Arc<WorkerSpawner> = Arc::new(|_, _, _| {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "injected worker spawn failure"))
    });
    start_inner_with_worker_spawner(state, stop, pending_loader, refresh, worker_spawner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pending_accounts_means_no_due_fetch() {
        let stop = Arc::new(AtomicBool::new(false));
        let queue = RefreshQueue::new(stop);
        assert!(queue.claim_ready(Instant::now()).is_none());
    }

    #[test]
    fn first_pending_account_is_claimed_immediately() {
        let queue = RefreshQueue::new(Arc::new(AtomicBool::new(false)));
        queue.enqueue("uid-a".into(), 1000, None);
        let item = queue.claim_ready(Instant::now()).expect("immediate first attempt");
        assert_eq!(item.account_ref, "uid-a");
        assert_eq!(item.generation, 1);
    }

    #[test]
    fn in_flight_new_request_coalesces_into_exactly_one_follow_up() {
        let queue = RefreshQueue::new(Arc::new(AtomicBool::new(false)));
        queue.enqueue("uid-a".into(), 1000, Some("req-first"));
        let first = queue.claim_ready(Instant::now()).unwrap();
        queue.enqueue("uid-a".into(), 2000, Some("req-next"));
        queue.enqueue("uid-a".into(), 2000, Some("req-next"));
        queue.finish(&first, true);
        let follow_up = queue.claim_ready(Instant::now()).expect("one immediate follow-up");
        assert!(follow_up.generation > first.generation);
        queue.finish(&follow_up, true);
        queue.enqueue("uid-a".into(), 1000, Some("req-next"));
        assert!(queue.claim_ready(Instant::now()).is_none(), "duplicate finalize must not reset backoff");
    }

    #[test]
    fn blocked_account_does_not_prevent_another_account_from_completing() {
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(RefreshQueue::new(stop.clone()));
        queue.enqueue("uid-slow".into(), 1000, None);
        queue.enqueue("uid-fast".into(), 1000, None);
        let (slow_started_tx, slow_started_rx) = std::sync::mpsc::channel();
        let (release_slow_tx, release_slow_rx) = std::sync::mpsc::channel();
        let release_slow_rx = Arc::new(Mutex::new(release_slow_rx));
        let (fast_done_tx, fast_done_rx) = std::sync::mpsc::channel();
        let slow_queue = queue.clone();
        let slow_release_rx = release_slow_rx.clone();
        let slow_worker = std::thread::spawn(move || run_worker(slow_queue, move |item| {
            assert_eq!(item.account_ref, "uid-slow");
            slow_started_tx.send(()).unwrap();
            slow_release_rx.lock().unwrap().recv().unwrap();
            true
        }));
        slow_started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let fast_queue = queue.clone();
        let fast_worker = std::thread::spawn(move || run_worker(fast_queue, move |item| {
            assert_eq!(item.account_ref, "uid-fast");
            fast_done_tx.send(()).unwrap();
            true
        }));
        fast_done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        stop.store(true, Ordering::Release);
        queue.wake_all();
        release_slow_tx.send(()).unwrap();
        slow_worker.join().unwrap();
        fast_worker.join().unwrap();
    }

    #[test]
    fn stopping_workers_prevents_any_new_fetch() {
        let stop = Arc::new(AtomicBool::new(true));
        let queue = Arc::new(RefreshQueue::new(stop));
        queue.enqueue("uid-a".into(), 1000, None);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_calls = calls.clone();
        run_worker(queue, move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            true
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

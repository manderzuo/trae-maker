use super::bridge_billing::{
    is_dirty_bridge_generation, is_recovery_required_generation, BridgeBillingStore,
};

const HOST_ACTIVITY_MUTEX: &str = "Global\\AIWorkBridgeBilling-host-v1";

/// Keep this guard alive until every charging worker using the instance has exited.
/// The Windows mutexes are owned by dedicated threads so this guard can be dropped
/// on a different worker thread without releasing a mutex from its non-owner.
pub(super) struct BridgeBudgetLease {
    instance_id: String,
    generation: String,
    recovery_required: bool,
    charge_ready: bool,
    closing: bool,
    closed: bool,
    #[cfg(windows)]
    host_lock: Option<WindowsNamedMutexGuard>,
    #[cfg(windows)]
    instance_lock: Option<WindowsNamedMutexGuard>,
}

impl BridgeBudgetLease {
    pub(super) fn try_acquire(store: &BridgeBillingStore) -> Result<Option<Self>, String> {
        Self::try_acquire_with_identity_hook(store, || {})
    }

    fn try_acquire_with_identity_hook<F>(
        store: &BridgeBillingStore,
        after_identity_read: F,
    ) -> Result<Option<Self>, String>
    where
        F: FnOnce(),
    {
        #[cfg(not(windows))]
        {
            let _ = store;
            after_identity_read();
            Err("bridge billing activity lease requires Windows; charging must stay disabled".into())
        }
        #[cfg(windows)]
        {
            let (instance_id, _) = store.bridge_identity()?;
            after_identity_read();
            // This conservative host-wide lock also covers independent v0 copies
            // that receive different persistent instance IDs during migration.
            let Some((host_lock, host_abandoned)) = WindowsNamedMutexGuard::try_acquire(
                HOST_ACTIVITY_MUTEX,
                "aiwork-bridge-host-lease",
            )? else {
                return Ok(None);
            };
            let (locked_instance_id, mut generation) = match store.bridge_identity() {
                Ok(identity) => identity,
                Err(error) if host_abandoned => {
                    std::mem::forget(host_lock);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            if locked_instance_id != instance_id {
                if host_abandoned {
                    std::mem::forget(host_lock);
                }
                return Err("bridge instance changed while acquiring the host activity mutex".into());
            }
            let mut recovery_required = is_dirty_bridge_generation(&generation);
            if host_abandoned && !is_recovery_required_generation(&generation) {
                match store.mark_recovery_required(&instance_id, &generation) {
                    Ok(marker) => {
                        generation = marker;
                        recovery_required = true;
                    }
                    Err(error) => {
                        // Do not release an abandoned fence if its durable marker
                        // could not be written; otherwise the next owner loses the
                        // WAIT_ABANDONED signal and may treat startup as clean.
                        std::mem::forget(host_lock);
                        return Err(error);
                    }
                }
            }
            // Global is shared across Windows sessions on the same host. The
            // stable DB identity, never a path or client value, defines contention.
            let instance_name = format!("Global\\AIWorkBridgeBilling-v1-{instance_id}");
            let Some((instance_lock, instance_abandoned)) = WindowsNamedMutexGuard::try_acquire(
                &instance_name,
                "aiwork-bridge-instance-lease",
            )? else {
                drop(host_lock);
                return Ok(None);
            };
            let (locked_instance_id, locked_generation) = match store.bridge_identity() {
                Ok(identity) => identity,
                Err(error) if host_abandoned || instance_abandoned => {
                    std::mem::forget(instance_lock);
                    std::mem::forget(host_lock);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            if locked_instance_id != instance_id {
                if host_abandoned || instance_abandoned {
                    std::mem::forget(instance_lock);
                    std::mem::forget(host_lock);
                }
                return Err("bridge instance changed while acquiring its activity mutex".into());
            }
            if locked_generation != generation {
                if is_dirty_bridge_generation(&locked_generation) {
                    generation = locked_generation;
                    recovery_required = true;
                } else {
                    return Err("bridge generation changed while both activity mutexes were held".into());
                }
            }
            if host_abandoned || instance_abandoned || recovery_required {
                match store.mark_recovery_required(&instance_id, &generation) {
                    Ok(marker) => {
                        generation = marker;
                        recovery_required = true;
                    }
                    Err(error) => {
                        // Keep both owners fenced until process exit if persistence
                        // fails, so an abandoned signal cannot be consumed silently.
                        std::mem::forget(instance_lock);
                        std::mem::forget(host_lock);
                        return Err(error);
                    }
                }
            } else {
                generation = store.mark_active_owner(&instance_id, &generation)?;
            }
            Ok(Some(Self {
                instance_id,
                generation,
                recovery_required,
                charge_ready: !recovery_required,
                closing: false,
                closed: false,
                host_lock: Some(host_lock),
                instance_lock: Some(instance_lock),
            }))
        }
    }

    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn generation(&self) -> &str {
        &self.generation
    }

    pub(super) fn record_recovered_generation(&mut self, generation: String) {
        self.generation = generation;
        self.recovery_required = false;
        self.charge_ready = true;
        self.closing = false;
        self.closed = false;
    }

    pub(super) fn begin_clean_close(&mut self) {
        self.closing = true;
        self.charge_ready = false;
    }

    pub(super) fn record_cleanly_closed_generation(&mut self, generation: String) {
        self.generation = generation;
        self.recovery_required = false;
        self.charge_ready = false;
        self.closing = false;
        self.closed = true;
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(super) fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub(super) fn charge_ready(&self) -> bool {
        self.charge_ready && self.is_active() && !self.recovery_required && !self.closing && !self.closed
    }

    pub(super) fn is_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.host_lock.as_ref().is_some_and(WindowsNamedMutexGuard::is_active)
                && self.instance_lock.as_ref().is_some_and(WindowsNamedMutexGuard::is_active)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(windows)]
struct WindowsNamedMutexGuard {
    release_tx: Option<std::sync::mpsc::Sender<()>>,
    owner: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl WindowsNamedMutexGuard {
    fn try_acquire(name: &str, owner_name: &str) -> Result<Option<(Self, bool)>, String> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            CreateMutexW, ReleaseMutex, WaitForSingleObject,
        };

        let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let owner = std::thread::Builder::new()
            .name(owner_name.to_string())
            .spawn(move || unsafe {
                let handle = CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr());
                if handle.is_null() {
                    let error = std::io::Error::last_os_error();
                    let _ = result_tx.send(Err(format!("bridge activity mutex unavailable: {error}")));
                    return;
                }
                match WaitForSingleObject(handle, 0) {
                    WAIT_OBJECT_0 => {
                        if result_tx.send(Ok(Some(false))).is_ok() {
                            let _ = release_rx.recv();
                        }
                        let _ = ReleaseMutex(handle);
                    }
                    WAIT_ABANDONED => {
                        if result_tx.send(Ok(Some(true))).is_ok() {
                            let _ = release_rx.recv();
                        }
                        let _ = ReleaseMutex(handle);
                    }
                    WAIT_TIMEOUT => {
                        let _ = result_tx.send(Ok(None));
                    }
                    _ => {
                        let error = std::io::Error::last_os_error();
                        let _ = result_tx.send(Err(format!("bridge activity mutex wait failed: {error}")));
                    }
                }
                let _ = CloseHandle(handle);
            })
            .map_err(|error| format!("bridge activity mutex owner unavailable: {error}"))?;

        match result_rx.recv() {
            Ok(Ok(Some(abandoned))) => Ok(Some((Self {
                release_tx: Some(release_tx),
                owner: Some(owner),
            }, abandoned))),
            Ok(Ok(None)) => {
                drop(release_tx);
                owner.join().map_err(|_| "bridge activity mutex owner panicked".to_string())?;
                Ok(None)
            }
            Ok(Err(error)) => {
                drop(release_tx);
                let _ = owner.join();
                Err(error)
            }
            Err(error) => {
                drop(release_tx);
                let _ = owner.join();
                Err(format!("bridge activity mutex owner exited unexpectedly: {error}"))
            }
        }
    }

    fn is_active(&self) -> bool {
        self.owner.as_ref().is_some_and(|owner| !owner.is_finished())
    }
}

#[cfg(windows)]
impl Drop for WindowsNamedMutexGuard {
    fn drop(&mut self) {
        self.release_tx.take();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

impl Drop for BridgeBudgetLease {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // Release in reverse acquisition order so another process never sees
            // the host lock free while this guard still owns the instance lock.
            self.instance_lock.take();
            self.host_lock.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BridgeBudgetLease;
    use super::super::bridge_billing::BridgeBillingStore;
    use std::path::PathBuf;
    #[cfg(windows)]
    use std::path::Path;

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aiwork-bridge-lease-{label}-{}", rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(windows)]
    fn child_probe(dir: &Path) -> (bool, String) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "api_server::bridge_budget_lease::tests::lease_child_probe",
                "--nocapture",
            ])
            .env("AIWORK_TASK3A1_LEASE_PROBE_DIR", dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        (output.status.success(), format!("{stdout}\n{stderr}"))
    }

    #[cfg(windows)]
    #[test]
    fn lease_child_probe() {
        let Ok(dir) = std::env::var("AIWORK_TASK3A1_LEASE_PROBE_DIR") else {
            return;
        };
        let store = BridgeBillingStore::open(Path::new(&dir)).unwrap();
        let verdict = if BridgeBudgetLease::try_acquire(&store).unwrap().is_some() {
            "ACQUIRED"
        } else {
            "BUSY"
        };
        println!("LEASE_PROBE_RESULT={verdict}");
    }

    #[cfg(windows)]
    #[test]
    fn lease_abandonment_child_holds_mutex_until_killed() {
        let (Ok(dir), Ok(ready_path)) = (
            std::env::var("AIWORK_TASK3A1_ABANDONMENT_DIR"),
            std::env::var("AIWORK_TASK3A1_ABANDONMENT_READY"),
        ) else {
            return;
        };
        let store = BridgeBillingStore::open(Path::new(&dir)).unwrap();
        let lease = BridgeBudgetLease::try_acquire(&store).unwrap()
            .expect("the child must acquire both activity mutexes");
        std::fs::write(ready_path, b"LOCKS_HELD").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            std::hint::black_box(&lease);
        }
    }

    #[cfg(windows)]
    #[test]
    fn process_crash_persists_dirty_lease_and_requires_explicit_recovery() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let dir = test_dir("abandoned-recovery-marker");
        let ready_path = dir.join("child-ready");
        let seed = BridgeBillingStore::open(&dir).unwrap();
        let before = seed.bridge_identity().unwrap().1;
        drop(seed);

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "api_server::bridge_budget_lease::tests::lease_abandonment_child_holds_mutex_until_killed",
                "--nocapture",
            ])
            .env("AIWORK_TASK3A1_ABANDONMENT_DIR", &dir)
            .env("AIWORK_TASK3A1_ABANDONMENT_READY", &ready_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut child_exit_before_ready = None;
        let mut poll_error = None;
        while !ready_path.exists() && Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => {
                    child_exit_before_ready = Some(status);
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    poll_error = Some(error.to_string());
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let child_signaled = ready_path.exists();
        let active_generation_while_child_held = if child_signaled {
            rusqlite::Connection::open(dir.join("bridge-billing.sqlite3")).and_then(|connection| {
                connection.query_row(
                    "SELECT event_generation FROM bridge_schema_meta WHERE singleton = 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
            })
        } else {
            Err(rusqlite::Error::InvalidQuery)
        };
        let child_status = if let Some(status) = child_exit_before_ready {
            Ok(status)
        } else {
            // On timeout, or if process polling failed, terminate before wait so
            // the test cannot block forever while the child still owns mutexes.
            let kill_result = child.kill();
            child.wait().map_err(|wait_error| match kill_result {
                Ok(()) => format!("child wait after termination failed: {wait_error}"),
                Err(kill_error) => format!(
                    "child termination failed ({kill_error}) and wait failed ({wait_error})"
                ),
            })
        };

        assert!(poll_error.is_none(), "child readiness polling failed: {poll_error:?}");
        let child_status = child_status.expect("child process must be reaped before assertions");
        let recovered = BridgeBillingStore::open(&dir).unwrap();
        let lease = BridgeBudgetLease::try_acquire(&recovered).unwrap()
            .expect("the abandoned mutexes must be reacquirable");
        let persisted_marker = recovered.bridge_identity().unwrap().1;
        let lease_generation = lease.generation().to_string();
        let lease_is_active = lease.is_active();
        let lease_is_charge_ready = lease.charge_ready();
        drop(lease);
        drop(recovered);

        let mut reopened = BridgeBillingStore::open(&dir).unwrap();
        let marker_after_reopen = reopened.bridge_identity().unwrap().1;
        let mut recovery_lease = BridgeBudgetLease::try_acquire(&reopened).unwrap()
            .expect("the persisted dirty lease must be fenced by the mutexes");
        let denied_recovery = reopened.rotate_event_generation_for_recovery(
            &mut recovery_lease,
            || Err("old workers are not confirmed stopped".into()),
        );
        let marker_after_denied_recovery = reopened.bridge_identity().unwrap().1;
        let ready_after_denied_recovery = recovery_lease.charge_ready();
        let recovered_generation = reopened.rotate_event_generation_for_recovery(
            &mut recovery_lease,
            || Ok(()),
        );
        let generation_after_recovery = reopened.bridge_identity().unwrap().1;
        let ready_after_recovery = recovery_lease.charge_ready();
        let denied_clean_close = reopened.finish_activity_lease_cleanly(
            &mut recovery_lease,
            || Err("old workers are still active".into()),
        );
        let generation_after_denied_close = reopened.bridge_identity().unwrap().1;
        let ready_after_denied_close = recovery_lease.charge_ready();
        let clean_generation = reopened.finish_activity_lease_cleanly(
            &mut recovery_lease,
            || Ok(()),
        );
        let generation_after_clean_close = reopened.bridge_identity().unwrap().1;
        let ready_after_clean_close = recovery_lease.charge_ready();
        drop(recovery_lease);
        drop(reopened);
        let clean_reopen = BridgeBillingStore::open(&dir).unwrap();
        let generation_after_clean_reopen = clean_reopen.bridge_identity().unwrap().1;
        drop(clean_reopen);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(child_signaled, "child did not acquire both mutexes within 15 seconds");
        assert!(!child_status.success(), "child must be force-terminated while owning mutexes");
        let active_generation_while_child_held = active_generation_while_child_held
            .expect("active owner state must be readable while the child holds its lease");
        assert!(active_generation_while_child_held.starts_with("bridge-active-v1-"),
            "a charge-ready lease must persist its active owner before returning, got {active_generation_while_child_held}");
        assert!(persisted_marker.starts_with("bridge-recovery-required-v1-"),
            "a stale active owner must be marked recovery-required even when mutex reacquisition returns WAIT_OBJECT_0, got {persisted_marker} (initially {before})");
        assert_eq!(lease_generation, persisted_marker, "lease must adopt the persisted pending generation");
        assert!(lease_is_active, "the acquired OS locks remain active while recovery is pending");
        assert!(!lease_is_charge_ready, "successful mutex acquisition cannot make a crashed owner charge-ready");
        assert_eq!(marker_after_reopen, persisted_marker,
            "a reopen must not silently clear the pending recovery marker");
        assert!(denied_recovery.is_err(), "failed worker-stop confirmation must retain pending state");
        assert_eq!(marker_after_denied_recovery, persisted_marker);
        assert!(!ready_after_denied_recovery, "recovery failure cannot make the lease charge-ready");
        assert!(recovered_generation.is_ok(), "explicit recovery confirmation should establish a new active owner");
        assert!(generation_after_recovery.starts_with("bridge-active-v1-"));
        assert!(ready_after_recovery, "the recovered current owner can become ready after explicit confirmation");
        assert!(denied_clean_close.is_err(), "failed clean-close confirmation must retain the dirty marker");
        assert_eq!(generation_after_denied_close, generation_after_recovery);
        assert!(!ready_after_denied_close, "clean close disables readiness before confirmation completes");
        assert!(clean_generation.is_ok(), "explicit clean close should clear the active marker");
        assert!(generation_after_clean_close.starts_with("bridge-generation-v1-"));
        assert!(!ready_after_clean_close, "a cleanly closed lease is no longer charge-ready");
        assert_eq!(generation_after_clean_reopen, generation_after_clean_close,
            "only explicit clean close may restore the idle generation across reopen");
    }

    #[cfg(windows)]
    #[test]
    fn lease_uses_generation_reread_after_mutex_barrier() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = test_dir("locked-generation-reread");
        let seed = BridgeBillingStore::open(&dir).unwrap();
        let (instance_id, original_generation) = seed.bridge_identity().unwrap();
        drop(seed);
        let marker = format!("bridge-recovery-required-v1-{}", "a".repeat(32));
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let contender_dir = dir.clone();
        let contender = std::thread::spawn(move || {
            let store = BridgeBillingStore::open(&contender_dir).unwrap();
            BridgeBudgetLease::try_acquire_with_identity_hook(&store, || {
                snapshot_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            }).map(|lease| lease.map(|lease| (
                lease.generation().to_string(), lease.is_active(), lease.charge_ready(),
            )))
        });

        let paused_before_mutex = snapshot_rx.recv_timeout(Duration::from_secs(10)).is_ok();
        let update_result = if paused_before_mutex {
            rusqlite::Connection::open(dir.join("bridge-billing.sqlite3"))
                .and_then(|connection| connection.execute(
                    "UPDATE bridge_schema_meta SET event_generation = ?1
                     WHERE singleton = 1 AND bridge_instance_id = ?2 AND event_generation = ?3",
                    rusqlite::params![marker, instance_id, original_generation],
                ))
        } else {
            Err(rusqlite::Error::InvalidQuery)
        };
        let _ = resume_tx.send(());
        let acquired = contender.join().unwrap();
        let acquired = acquired.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(paused_before_mutex, "contender did not reach the controlled pre-lock barrier");
        assert_eq!(update_result.unwrap(), 1, "fixture must change generation while contender is paused");
        let (lease_generation, lease_active, lease_charge_ready) = acquired
            .expect("contender should acquire the mutexes after the fixture update");
        assert_eq!(lease_generation, marker, "lease must use the generation reread under both mutexes");
        assert!(lease_active, "the mutex lease itself remains active during recovery");
        assert!(!lease_charge_ready, "a newly observed pending marker must fail closed");
    }

    #[cfg(windows)]
    #[test]
    fn copied_databases_allow_only_one_active_lease_across_processes() {
        let root = test_dir("copied-processes");
        let primary = root.join("primary");
        let replica = root.join("replica");
        let seed = BridgeBillingStore::open(&primary).unwrap();
        drop(seed);
        std::fs::create_dir_all(&replica).unwrap();
        let checkpoint = rusqlite::Connection::open(primary.join("bridge-billing.sqlite3")).unwrap();
        checkpoint.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(checkpoint);
        std::fs::copy(
            primary.join("bridge-billing.sqlite3"), replica.join("bridge-billing.sqlite3")
        ).unwrap();
        let first = BridgeBillingStore::open(&primary).unwrap();
        let second = BridgeBillingStore::open(&replica).unwrap();
        let first_active = BridgeBudgetLease::try_acquire(&first).unwrap().unwrap();
        let second_attempt = BridgeBudgetLease::try_acquire(&second).unwrap();
        let second_was_denied = second_attempt.is_none();
        drop(second_attempt);
        let (busy_child_ok, busy_child_output) = child_probe(&replica);
        drop(first_active);
        let (free_child_ok, free_child_output) = child_probe(&replica);
        drop(second);
        drop(first);
        std::fs::remove_dir_all(&root).unwrap();

        assert!(second_was_denied, "a copied DB must contend on the same instance lease");
        assert!(busy_child_ok && busy_child_output.contains("LEASE_PROBE_RESULT=BUSY"),
            "other process gained activity while lease held: {busy_child_output}");
        assert!(free_child_ok && free_child_output.contains("LEASE_PROBE_RESULT=ACQUIRED"),
            "other process could not acquire after release: {free_child_output}");
    }

    #[cfg(windows)]
    #[test]
    fn dropping_active_guard_allows_reacquisition() {
        let dir = test_dir("release-reacquire");
        let store = BridgeBillingStore::open(&dir).unwrap();
        let first = BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        let concurrent = BridgeBudgetLease::try_acquire(&store).unwrap();
        let concurrent_denied = concurrent.is_none();
        drop(concurrent);
        drop(first);
        let reacquired = BridgeBudgetLease::try_acquire(&store).unwrap().is_some();
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(concurrent_denied, "the active guard must exclude a second holder");
        assert!(reacquired, "release must permit a later holder");
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_activity_lease_fails_closed() {
        let dir = test_dir("unsupported-platform");
        let store = BridgeBillingStore::open(&dir).unwrap();
        let result = BridgeBudgetLease::try_acquire(&store);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err(), "charging activity cannot proceed without a Windows fence");
    }
}

use super::bridge_billing::BridgeBillingStore;

/// Keep this guard alive until every charging worker using the instance has exited.
/// The Windows mutex is owned by a dedicated thread so the guard can be dropped
/// on a different worker thread without releasing a mutex from its non-owner.
pub(super) struct BridgeBudgetLease {
    instance_id: String,
    generation: String,
    #[cfg(windows)] release_tx: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(windows)] owner: Option<std::thread::JoinHandle<()>>,
}

impl BridgeBudgetLease {
    pub(super) fn try_acquire(store: &BridgeBillingStore) -> Result<Option<Self>, String> {
        #[cfg(not(windows))]
        {
            let _ = store;
            Err("bridge billing activity lease requires Windows; charging must stay disabled".into())
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

            let (instance_id, generation) = store.bridge_identity()?;
            // Global is shared across Windows sessions on the same host. The
            // stable DB identity, never a path or client value, defines contention.
            let name = format!("Global\\AIWorkBridgeBilling-v1-{instance_id}");
            let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let owner = std::thread::Builder::new()
                .name("aiwork-bridge-lease".into())
                .spawn(move || unsafe {
                    let handle = CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr());
                    if handle.is_null() {
                        let error = std::io::Error::last_os_error();
                        let _ = result_tx.send(Err(format!("bridge activity mutex unavailable: {error}")));
                        return;
                    }
                    match WaitForSingleObject(handle, 0) {
                        WAIT_OBJECT_0 | WAIT_ABANDONED => {
                            if result_tx.send(Ok(true)).is_ok() {
                                let _ = release_rx.recv();
                            }
                            let _ = ReleaseMutex(handle);
                        }
                        WAIT_TIMEOUT => {
                            let _ = result_tx.send(Ok(false));
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
                Ok(Ok(true)) => Ok(Some(Self {
                    instance_id,
                    generation,
                    release_tx: Some(release_tx),
                    owner: Some(owner),
                })),
                Ok(Ok(false)) => {
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
    }

    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn generation(&self) -> &str {
        &self.generation
    }

    pub(super) fn record_generation(&mut self, generation: String) {
        self.generation = generation;
    }

    pub(super) fn is_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.owner.as_ref().is_some_and(|owner| !owner.is_finished())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

impl Drop for BridgeBudgetLease {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.release_tx.take();
            if let Some(owner) = self.owner.take() {
                let _ = owner.join();
            }
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

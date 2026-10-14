//! Recover only files for an already completed and authenticated execution.
use std::{collections::HashMap,path::{Path,PathBuf},sync::{Arc,Condvar,Mutex,OnceLock},time::Instant};
use super::video_cache::CachedFile as File;

#[derive(Default)]
struct Recovery {result:Mutex<Option<Result<(),String>>>,ready:Condvar}
static RECOVERING:OnceLock<Mutex<HashMap<PathBuf,Arc<Recovery>>>>=OnceLock::new();
struct RecoveryGuard {path:PathBuf,shared:Arc<Recovery>}
impl RecoveryGuard {
    fn publish(&self,result:Result<(),String>) {
        *self.shared.result.lock().unwrap_or_else(|e|e.into_inner())=Some(result);
        self.shared.ready.notify_all();
    }
}
impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        let mut result=self.shared.result.lock().unwrap_or_else(|e|e.into_inner());
        if result.is_none() {*result=Some(Err("artifact recovery interrupted".into()));}
        self.shared.ready.notify_all();drop(result);
        RECOVERING.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).remove(&self.path);
    }
}
fn open_valid(path:&Path)->Result<(File,u64),String> {
    File::open(path)
}
fn open_or_fetch(data_dir:&Path,task:&str,fetch:impl FnOnce()->Result<(),String>)->Result<(File,u64),String> {
    #[cfg(test)]
    assert!(std::env::var_os("AIWORK_VIDEO_DIR").is_none(),"artifact tests must use isolated storage");
    let path=super::video_store::artifact_path(data_dir,task)?;
    std::fs::create_dir_all(path.parent().ok_or("artifact path invalid")?).map_err(|_|"artifact directory unavailable")?;
    // Hold through recovery and into CachedFile's own lease; publication must
    // not create a cleanup gap before the authenticated reader opens it.
    let _lease=super::video_cache::Lease::acquire(&path)?;
    if let Ok(file)=open_valid(&path) {return Ok(file);}
    let (shared,owner)={
        let mut busy=RECOVERING.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner());
        if let Some(shared)=busy.get(&path) {(shared.clone(),false)} else {
            if busy.len()>=4 {return Err("artifact_recovery_busy".into());}
            let shared=Arc::new(Recovery::default());busy.insert(path.clone(),shared.clone());(shared,true)
        }
    };
    if !owner {
        // Readers of one immutable artifact share one download. They never
        // create paid work or restart the failed owner's network operation.
        let deadline=Instant::now()+super::native_io::ARTIFACT_LIMIT;
        let mut result=shared.result.lock().unwrap_or_else(|e|e.into_inner());
        loop {
            if let Some(result)=result.as_ref() {result.clone()?;return open_valid(&path);}
            let remaining=deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {return Err("artifact_recovery_busy".into());}
            result=shared.ready.wait_timeout(result,remaining).unwrap_or_else(|e|e.into_inner()).0;
        }
    }
    let guard=RecoveryGuard {path:path.clone(),shared};
    // A previous owner may have published the file before this slot was acquired.
    let result=if let Ok(file)=open_valid(&path) {Ok(file)} else {fetch().and_then(|()|open_valid(&path))};
    guard.publish(result.as_ref().map(|_|()).map_err(Clone::clone));
    result
}

/// The caller has authenticated the bridge and checked the persisted execution,
/// request and encrypted result. No client-supplied URL enters this function.
pub(super) fn open_completed(state:&super::ApiSharedState,account_ref:&str,task:&str,result:&serde_json::Value)->Result<(File,u64),String> {
    if result["id"].as_str()!=Some(task) || result["status"]!="completed" {return Err("artifact result binding mismatch".into());}
    open_or_fetch(&state.data_dir,task,|| {
        let budget=super::native_io::Budget::new(super::native_io::ARTIFACT_LIMIT);
        let refreshed=result["resource_uri"].as_str().filter(|s|!s.is_empty() && s.len()<=2048)
            .and_then(|uri|state.pool.completed_resource_credentials(account_ref)
                .and_then(|account|super::video::resolve_resource_url_until(&account,uri,budget.capped(std::time::Duration::from_secs(20))).ok()));
        let url=refreshed.as_deref().or_else(||result["video_url"].as_str()).ok_or("artifact source unavailable")?;
        if !url.starts_with("https://") {return Err("artifact source is not HTTPS".into());}
        super::video_store::download_until(&state.data_dir,task,url,budget).map(|_|())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {fn drop(&mut self) {let _=std::fs::remove_dir_all(&self.0);}}

    #[test]
    fn cached_video_is_reused_and_failed_fetch_can_retry_without_paid_work() {
        let dir=Directory(std::env::temp_dir().join(format!("budget-artifact-{:032x}",rand::random::<u128>())));
        assert!(open_or_fetch(&dir.0,"video-one",||Err("transient cache failure".into())).is_err());
        let path=super::super::video_store::artifact_path(&dir.0,"video-one").unwrap();
        let (file,size)=open_or_fetch(&dir.0,"video-one",|| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();std::fs::write(&path,b"existing-video").unwrap();Ok(())
        }).unwrap();drop(file);assert_eq!(size,14);
        let (file,size)=open_or_fetch(&dir.0,"video-one",||panic!("a cached file must not perform another fetch")).unwrap();
        assert_eq!(size,14);drop(file);
        std::fs::write(&path,b"").unwrap();
        assert!(open_or_fetch(&dir.0,"video-one",||Ok(())).is_err(),"empty output is not a recovered artifact");
        assert!(open_or_fetch(&dir.0,"../escape",||panic!("invalid task must fail before I/O")).is_err());
    }

    #[test]
    fn concurrent_artifact_recovery_is_bounded_and_unwind_releases_its_slot() {
        let dir=Directory(std::env::temp_dir().join(format!("budget-artifact-bound-{:032x}",rand::random::<u128>())));
        let (ready,receive)=std::sync::mpsc::channel();
        let mut releases=Vec::new();let mut joins=Vec::new();
        for index in 0..4 {
            let (release,wait)=std::sync::mpsc::channel();releases.push(release);
            let ready=ready.clone();let path=dir.0.clone();
            joins.push(std::thread::spawn(move ||open_or_fetch(&path,&format!("video-{index}"),|| {
                ready.send(()).unwrap();wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();Err("fixture end".into())
            })));
        }
        for _ in 0..4 {receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap();}
        assert!(open_or_fetch(&dir.0,"video-extra",||panic!("unbounded network worker")).is_err());
        for release in releases {release.send(()).unwrap();}for join in joins {assert!(join.join().unwrap().is_err());}
        assert!(std::panic::catch_unwind(||open_or_fetch(&dir.0,"video-0",||panic!("fixture fetch panic"))).is_err());
        let mut retried=false;
        assert!(open_or_fetch(&dir.0,"video-0",||{retried=true;Err("retry reached".into())}).is_err());
        assert!(retried,"panic must not leave a permanently occupied recovery slot");
    }

    #[test]
    fn concurrent_readers_join_one_download_and_reuse_its_published_file() {
        let dir=Directory(std::env::temp_dir().join(format!("budget-artifact-join-{:032x}",rand::random::<u128>())));
        let (ready,receive)=std::sync::mpsc::channel();
        let (release,wait)=std::sync::mpsc::channel();
        let owner_dir=dir.0.clone();
        let owner=std::thread::spawn(move ||open_or_fetch(&owner_dir,"video-joined",|| {
            ready.send(()).unwrap();wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let path=super::super::video_store::artifact_path(&owner_dir,"video-joined").unwrap();
            std::fs::write(path,b"shared-result").unwrap();Ok(())
        }));
        receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let waiter_dir=dir.0.clone();
        let (done,received)=std::sync::mpsc::channel();
        let waiter=std::thread::spawn(move || {
            let result=open_or_fetch(&waiter_dir,"video-joined",||panic!("a joined reader must never fetch"));
            done.send(result.map(|(file,size)|{drop(file);size})).unwrap();
        });
        assert!(received.recv_timeout(std::time::Duration::from_millis(100)).is_err(),"reader should await publication instead of returning 429");
        release.send(()).unwrap();
        assert_eq!(received.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap(),13);
        drop(owner.join().unwrap().unwrap());waiter.join().unwrap();
    }
}

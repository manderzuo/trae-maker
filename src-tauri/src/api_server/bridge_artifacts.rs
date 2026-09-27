//! Recover only files for an already completed and authenticated execution.
use std::{collections::HashSet,fs::File,path::{Path,PathBuf},sync::{Mutex,OnceLock}};

static RECOVERING:OnceLock<Mutex<HashSet<PathBuf>>>=OnceLock::new();
struct RecoveryGuard(PathBuf);
impl Drop for RecoveryGuard {
    fn drop(&mut self) {RECOVERING.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).remove(&self.0);}
}
fn open_valid(path:&Path)->Result<(File,u64),String> {
    let file=File::open(path).map_err(|_|"artifact unavailable")?;
    let size=file.metadata().map_err(|_|"artifact unavailable")?.len();
    if size==0 || size>super::video_store::MAX_VIDEO_BYTES {return Err("artifact length invalid".into());}
    Ok((file,size))
}
fn open_or_fetch(data_dir:&Path,task:&str,fetch:impl FnOnce()->Result<(),String>)->Result<(File,u64),String> {
    let path=super::video_store::artifact_path(data_dir,task)?;
    if let Ok(file)=open_valid(&path) {return Ok(file);}
    {
        let mut busy=RECOVERING.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner());
        if busy.len()>=4 || !busy.insert(path.clone()) {return Err("artifact_recovery_busy".into());}
    }
    let _guard=RecoveryGuard(path.clone());
    // A previous owner may have published the file before this slot was acquired.
    if let Ok(file)=open_valid(&path) {return Ok(file);}
    fetch()?;
    open_valid(&path)
}

/// The caller has authenticated the bridge and checked the persisted execution,
/// request and encrypted result. No client-supplied URL enters this function.
pub(super) fn open_completed(state:&super::ApiSharedState,account_ref:&str,task:&str,result:&serde_json::Value)->Result<(File,u64),String> {
    if result["id"].as_str()!=Some(task) || result["status"]!="completed" {return Err("artifact result binding mismatch".into());}
    open_or_fetch(&state.data_dir,task,|| {
        let refreshed=result["resource_uri"].as_str().filter(|s|!s.is_empty() && s.len()<=2048)
            .and_then(|uri|state.pool.completed_resource_credentials(account_ref)
                .and_then(|account|super::video::resolve_resource_url(&account,uri).ok()));
        let url=refreshed.as_deref().or_else(||result["video_url"].as_str()).ok_or("artifact source unavailable")?;
        if !url.starts_with("https://") {return Err("artifact source is not HTTPS".into());}
        super::video_store::download_from_url(&state.data_dir,task,url).map(|_|())
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
        assert!(open_or_fetch(&dir.0,"video-0",||panic!("same artifact fetched twice")).is_err());
        assert!(open_or_fetch(&dir.0,"video-extra",||panic!("unbounded network worker")).is_err());
        for release in releases {release.send(()).unwrap();}for join in joins {assert!(join.join().unwrap().is_err());}
        assert!(std::panic::catch_unwind(||open_or_fetch(&dir.0,"video-0",||panic!("fixture fetch panic"))).is_err());
        let mut retried=false;
        assert!(open_or_fetch(&dir.0,"video-0",||{retried=true;Err("retry reached".into())}).is_err());
        assert!(retried,"panic must not leave a permanently occupied recovery slot");
    }
}

//! Volatile, bounded delta delivery. Durable execution results remain the
//! recovery source; no observer can backpressure a paid upstream worker.

use std::{collections::{HashMap,VecDeque},sync::{Arc,Mutex},time::{Duration,Instant}};
use serde_json::Value;
const MAX_BYTES:usize=1024*1024;
#[derive(Default,Clone)]
pub(super) struct Hub(Arc<Mutex<HashMap<String,Entry>>>);
struct Entry {chunks:VecDeque<(Value,usize)>,bytes:usize,first:u64,next:u64,finished:Option<Instant>}
pub(super) struct Writer {hub:Hub,id:String}
#[derive(serde::Serialize)]
pub(super) struct Page {pub chunks:Vec<Value>,pub next:u64,pub finished:bool}
impl Hub {
    pub(super) fn begin(&self,id:&str)->Result<Writer,String> {
        let mut all=self.0.lock().map_err(|_|"chat_chunk_cache_unavailable")?;
        all.retain(|_,e|!e.finished.is_some_and(|t|t.elapsed()>Duration::from_secs(120)));
        if all.contains_key(id) {return Err("chat_chunk_stream_exists".into());}
        if all.len()>=64 {
            let oldest=all.iter().filter_map(|(k,v)|v.finished.map(|t|(k.clone(),t))).min_by_key(|(_,t)|*t);
            if let Some((id,_))=oldest {all.remove(&id);} else {return Err("chat_chunk_cache_busy".into());}
        }
        all.insert(id.into(),Entry {chunks:VecDeque::new(),bytes:0,first:0,next:0,finished:None});
        Ok(Writer {hub:self.clone(),id:id.into()})
    }
    pub(super) fn page(&self,id:&str,after:u64)->Result<Option<Page>,String> {
        let all=self.0.lock().map_err(|_|"chat_chunk_cache_unavailable")?;
        let Some(e)=all.get(id) else {return Ok(None)};
        if after<e.first || after>e.next {return Err("chat_stream_cursor_unavailable".into());}
        let chunks=e.chunks.iter().skip((after-e.first) as usize).take(32).map(|(v,_)|v.clone()).collect::<Vec<_>>();
        Ok(Some(Page {next:after+chunks.len() as u64,chunks,finished:e.finished.is_some()}))
    }
}
impl Writer {
    pub(super) fn push(&self,value:Value) {
        let size=match serde_json::to_vec(&value) {Ok(b)=>b.len(),Err(_)=>return};
        let Ok(mut all)=self.hub.0.lock() else {return};let Some(e)=all.get_mut(&self.id) else {return};
        if e.finished.is_some() {return;}
        e.next+=1;
        if size>MAX_BYTES {e.chunks.clear();e.bytes=0;e.first=e.next;return;}
        while e.bytes+size>MAX_BYTES || e.chunks.len()>=4096 {
            if let Some((_,n))=e.chunks.pop_front() {e.bytes-=n;e.first+=1;} else {break;}
        }
        e.bytes+=size;e.chunks.push_back((value,size));
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(mut all)=self.hub.0.lock() {if let Some(e)=all.get_mut(&self.id) {e.finished=Some(Instant::now());}}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn separate_streams_detect_lag_and_keep_workers_nonblocking() {
        let hub=Hub::default();let a=hub.begin("a").unwrap();let b=hub.begin("b").unwrap();
        a.push(json!({"content":"one"}));b.push(json!({"content":"private"}));
        assert_eq!(hub.page("a",0).unwrap().unwrap().chunks[0]["content"],"one");
        assert_eq!(hub.page("b",0).unwrap().unwrap().chunks[0]["content"],"private");
        assert!(hub.page("a",2).is_err());assert!(hub.begin("a").is_err());
        for _ in 0..20 {a.push(json!({"content":"x".repeat(100_000)}));}
        assert!(hub.page("a",0).is_err(),"never hide dropped deltas");
        let page=hub.page("a",20).unwrap().unwrap();assert_eq!(page.next,21);assert!(!page.finished);
        drop(a);assert!(hub.page("a",21).unwrap().unwrap().finished);
        assert!(hub.page("missing",0).unwrap().is_none());
    }
    #[test]
    fn full_active_capacity_rejects_and_finished_entries_can_be_reclaimed() {
        let hub=Hub::default();let mut writers=Vec::new();
        for i in 0..64 {writers.push(hub.begin(&i.to_string()).unwrap());}
        assert!(hub.begin("extra").is_err());drop(writers.pop());
        assert!(hub.begin("extra").is_ok());assert!(hub.page("63",0).unwrap().is_none());
    }
}

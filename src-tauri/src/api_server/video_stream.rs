//! Video-only SSE consumer. Stream faults never prove zero charge.
use std::{io::{self,BufRead},time::Instant};
use super::bridge_diagnostics::TransportDiagnostic;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fault { Read, Timeout, Incomplete, InvalidEvent, TooLarge }

pub(super) const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const MAX_EVENT_BYTES: usize = 1024 * 1024;
impl Fault {
    pub(super) fn diagnostic(&self) -> super::bridge_diagnostics::Diagnostic {
        let code = match self {
            Self::Read => "video_stream_read_failed",
            Self::Timeout => "video_stream_timeout",
            Self::Incomplete => "video_stream_incomplete",
            Self::InvalidEvent => "video_stream_invalid_event",
            Self::TooLarge => "video_stream_too_large",
        };
        super::bridge_diagnostics::Diagnostic { code: code.into(), stage: "video_stream".into(), outcome_known: false, upstream_error: None, transport:None }
    }
}
pub(super) fn post(agent: &ureq::Agent, url: &str, timeout: std::time::Duration) -> ureq::Request {
    // ureq's deadline is carried into into_reader(), including each blocking
    // socket read. Heartbeats cannot restart it. Only video generation uses it.
    super::video_protocol::post(agent, url).timeout(timeout)
}
#[derive(Debug)]
pub(super) struct StreamFailure {pub fault:Fault,transport:TransportDiagnostic}
impl StreamFailure {
    pub(super) fn diagnostic(mut self,response:&ResponseInfo)->super::bridge_diagnostics::Diagnostic {
        self.transport.http_status=Some(response.status);
        self.transport.response_content_type=response.content_type.clone();
        let mut diagnostic=self.fault.diagnostic();diagnostic.transport=Some(self.transport);diagnostic
    }
}
pub(super) struct ResponseInfo {status:u16,content_type:Option<String>}
impl ResponseInfo {
    pub(super) fn capture(response:&ureq::Response)->Self {
        let content_type=response.header("content-type").map(|value| match value.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "text/event-stream"=>"text/event-stream", "application/json"=>"application/json", _=>"other",
        }.to_owned());
        Self {status:response.status(),content_type}
    }
}
struct StreamState {started:Instant,deadline:Instant,transport:TransportDiagnostic}
impl StreamState {
    fn fail(&self,fault:Fault)->StreamFailure {
        let mut transport=self.transport.clone();transport.elapsed_ms=self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        transport.deadline_reached=Instant::now()>=self.deadline;StreamFailure {fault,transport}
    }
    fn record_io(&mut self,error:&io::Error)->io::ErrorKind {
        use std::error::Error;
        self.transport.last_io_kind=Some(format!("{:?}",error.kind()));self.transport.cause_io_kinds.clear();
        self.transport.raw_os_error=error.raw_os_error();
        let mut effective=error.kind();let mut current: &(dyn Error+'static)=error;
        for _ in 0..8 {
            let next=if let Some(io)=current.downcast_ref::<io::Error>() {
                self.transport.cause_io_kinds.push(format!("{:?}",io.kind()));
                if self.transport.raw_os_error.is_none() {self.transport.raw_os_error=io.raw_os_error();}
                if effective==io::ErrorKind::Other {effective=io.kind();}
                io.get_ref().map(|source|source as &(dyn Error+'static))
            } else {current.source()};
            match next {Some(source)=>current=source,None=>break}
        }
        effective
    }
    fn line(&mut self,reader:&mut impl BufRead,frame_bytes:usize)->Result<Vec<u8>,StreamFailure> {
        let mut bytes=Vec::new();
        loop {
            if Instant::now()>=self.deadline {return Err(self.fail(Fault::Timeout));}
            let buffer=match reader.fill_buf() {
                Ok(buffer)=>buffer,
                Err(error)=>{
                    let kind=self.record_io(&error);
                    // This retries only a read on the SAME response, never a
                    // POST, new connection, account switch or paid execution.
                    let transient=matches!(kind,io::ErrorKind::Interrupted|io::ErrorKind::TimedOut|io::ErrorKind::WouldBlock);
                    if transient && self.transport.read_retry_count==0 && Instant::now()<self.deadline {
                        self.transport.read_retry_count=1;
                        continue;
                    }
                    return Err(self.fail(if matches!(kind,io::ErrorKind::TimedOut|io::ErrorKind::WouldBlock) {Fault::Timeout}else{Fault::Read}));
                }
            };
            if buffer.is_empty() {return Ok(bytes);}
            let end=buffer.iter().position(|byte|*byte==b'\n').map(|n|n+1);
            let cap=MAX_EVENT_BYTES.saturating_sub(frame_bytes+bytes.len())+1;
            let count=end.unwrap_or(buffer.len()).min(cap);let complete=end==Some(count);
            bytes.extend_from_slice(&buffer[..count]);reader.consume(count);
            self.transport.bytes_received=self.transport.bytes_received.saturating_add(count as u64);
            if frame_bytes+bytes.len()>MAX_EVENT_BYTES {return Err(self.fail(Fault::TooLarge));}
            if complete {return Ok(bytes);}
        }
    }
}
pub(super) fn consume_detailed(mut reader: impl BufRead, deadline: Instant,
    mut event: impl FnMut(&str, &str) -> Result<bool, String>) -> Result<(), StreamFailure> {
    let mut state=StreamState {started:Instant::now(),deadline,transport:TransportDiagnostic::default()};
    let mut name = String::new();
    let mut data = String::new();
    let mut frame_bytes = 0usize;
    loop {
        let bytes=state.line(&mut reader,frame_bytes)?;
        if Instant::now() >= deadline { return Err(state.fail(Fault::Timeout)); }
        if bytes.is_empty() {
            if !data.is_empty() {
                state.transport.events_received+=1;
                if event(&name,&data).map_err(|_|state.fail(Fault::InvalidEvent))? {return Ok(());}
            }
            return Err(state.fail(Fault::Incomplete));
        }
        frame_bytes+=bytes.len();
        let line = std::str::from_utf8(&bytes).map_err(|_|state.fail(Fault::InvalidEvent))?.trim_end_matches(['\r','\n']);
        if line.trim().is_empty() {
            if !data.is_empty() {
                state.transport.events_received+=1;
                if event(&name,&data).map_err(|_|state.fail(Fault::InvalidEvent))? {return Ok(());}
            }
            name.clear(); data.clear(); frame_bytes = 0;
        } else if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() { data.push('\n'); }
            data.push_str(value.trim_start());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read, BufReader};
    fn consume(reader:impl BufRead,deadline:Instant,event:impl FnMut(&str,&str)->Result<bool,String>)->Result<(),Fault> {
        consume_detailed(reader,deadline,event).map_err(|failure|failure.fault)
    }
    struct Broken { reads: usize }
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads <= 4 { Err(io::Error::new(io::ErrorKind::InvalidData, "private upstream error")) } else { Ok(0) }
        }
    }
    #[test]
    fn persistent_read_error_stops_on_first_error() {
        let mut source = Broken { reads: 0 };
        assert_eq!(consume(BufReader::new(&mut source), std::time::Instant::now() + MAX_WAIT, |_, _| panic!("no valid event")), Err(Fault::Read));
        assert_eq!(source.reads, 1);
    }
    fn deadline() -> std::time::Instant { std::time::Instant::now() + MAX_WAIT }
    struct Glitch {
        prefix: io::Cursor<Vec<u8>>,
        suffix: io::Cursor<Vec<u8>>,
        errors: std::collections::VecDeque<io::ErrorKind>,
        error_reads: usize,
    }
    impl Read for Glitch {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.prefix.position() < self.prefix.get_ref().len() as u64 { return self.prefix.read(out); }
            if let Some(kind) = self.errors.pop_front() {
                self.error_reads += 1;
                return Err(io::Error::new(kind, "token=private https://private.invalid/signed"));
            }
            self.suffix.read(out)
        }
    }
    fn glitch(errors: &[io::ErrorKind]) -> Glitch {
        Glitch { prefix: io::Cursor::new(b"event: result\ndata: {\"uri\":\"tos://".to_vec()),
            suffix: io::Cursor::new(b"safe\"}\n\nevent: done\ndata: {}\n\n".to_vec()), errors: errors.iter().copied().collect(), error_reads: 0 }
    }
    #[test]
    fn one_transient_read_recovers_the_same_partial_event_without_replaying() {
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
            let mut source = glitch(&[kind]); let mut observed = Vec::new();
            let result = consume(BufReader::new(&mut source), deadline(), |name, data| {
                observed.push((name.to_owned(), data.to_owned())); Ok(name == "done")
            });
            assert_eq!(result, Ok(()), "transient kind {kind:?}");
            assert_eq!(source.error_reads, 1);
            assert_eq!(observed, vec![("result".into(), r#"{"uri":"tos://safe"}"#.into()), ("done".into(), "{}".into())]);
        }
    }
    #[test]
    fn a_second_transient_read_stops_without_losing_the_unknown_outcome_boundary() {
        let mut source = glitch(&[io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock]);
        assert_eq!(consume(BufReader::new(&mut source), deadline(), |_, _| panic!("no complete event")), Err(Fault::Timeout));
        assert_eq!(source.error_reads, 2);
    }
    #[test]
    fn damaged_or_disconnected_streams_are_not_retried_even_if_fixture_has_more_data() {
        for kind in [io::ErrorKind::InvalidData, io::ErrorKind::UnexpectedEof, io::ErrorKind::ConnectionReset, io::ErrorKind::Other] {
            let mut source = glitch(&[kind]);
            assert_eq!(consume(BufReader::new(&mut source), deadline(), |_, _| panic!("untrusted continuation")), Err(Fault::Read));
            assert_eq!(source.error_reads, 1);
        }
    }
    #[test]
    fn diagnostics_keep_closed_io_facts_and_partial_byte_counts_without_private_text() {
        let mut source=glitch(&[io::ErrorKind::InvalidData]);
        let error=consume_detailed(BufReader::new(&mut source),deadline(),|_,_|Ok(false)).unwrap_err();
        let response:ureq::Response="HTTP/1.1 200 OK\r\nContent-Type: private-token\r\n\r\n".parse().unwrap();
        let diagnostic=error.diagnostic(&ResponseInfo::capture(&response));
        assert_eq!(diagnostic.code,"video_stream_read_failed");assert!(!diagnostic.outcome_known);
        let detail=diagnostic.transport.as_ref().unwrap();
        assert_eq!(detail.last_io_kind.as_deref(),Some("InvalidData"));assert_eq!(detail.read_retry_count,0);
        assert_eq!(detail.bytes_received,34);assert_eq!(detail.events_received,0);
        assert_eq!(detail.http_status,Some(200));assert_eq!(detail.response_content_type.as_deref(),Some("other"));
        let json=serde_json::to_string(&diagnostic).unwrap();
        for private in ["private","https://","tos://","token="] {assert!(!json.contains(private));}
    }
    #[test]
    fn wrapped_transient_io_is_classified_without_retrying_fatal_wrapped_io() {
        struct Once {error:Option<io::Error>,tail:io::Cursor<Vec<u8>>}
        impl Read for Once {
            fn read(&mut self,out:&mut[u8])->io::Result<usize> {
                if let Some(error)=self.error.take() {Err(error)}else{self.tail.read(out)}
            }
        }
        for (kind,recover) in [(io::ErrorKind::TimedOut,true),(io::ErrorKind::InvalidData,false)] {
            let reader=Once {error:Some(io::Error::new(io::ErrorKind::Other,io::Error::new(kind,"private"))),tail:io::Cursor::new(b"event: done\ndata: {}\n\n".to_vec())};
            let result=consume_detailed(BufReader::new(reader),deadline(),|name,_|Ok(name=="done"));
            assert_eq!(result.is_ok(),recover);
            if let Err(error)=result {assert_eq!(error.transport.cause_io_kinds,vec!["Other","InvalidData"]);}
        }
    }
    #[test]
    fn raw_os_error_is_kept_without_its_localized_message() {
        let mut state=StreamState {started:Instant::now(),deadline:deadline(),transport:TransportDiagnostic::default()};
        state.record_io(&io::Error::from_raw_os_error(10054));
        assert_eq!(state.transport.raw_os_error,Some(10054));
        assert_eq!(state.transport.read_retry_count,0);
    }
    #[test]
    fn real_http_chunk_framing_fault_preserves_unknown_and_http_context() {
        use std::{net::TcpListener,io::Write};
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let url=format!("http://{}/fixture",listener.local_addr().unwrap());
        let server=std::thread::spawn(move|| {
            let (mut socket,_)=listener.accept().unwrap();socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut request=Vec::new();
            while !request.ends_with(b"\r\n\r\n") {let mut byte=[0];socket.read_exact(&mut byte).unwrap();request.push(byte[0]);}
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nnot-a-chunk\r\n").unwrap();
        });
        let agent=ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let response=post(&agent,&url,std::time::Duration::from_secs(2)).call().unwrap();
        let info=ResponseInfo::capture(&response);
        let failure=consume_detailed(BufReader::new(response.into_reader()),deadline(),|_,_|panic!("invalid framing must not be a result")).unwrap_err();
        let diagnostic=failure.diagnostic(&info);server.join().unwrap();
        assert!(!diagnostic.outcome_known);let detail=diagnostic.transport.unwrap();
        assert!(detail.last_io_kind.is_some());assert_eq!(detail.read_retry_count,0);
        assert_eq!(detail.http_status,Some(200));assert_eq!(detail.response_content_type.as_deref(),Some("text/event-stream"));
    }
    #[test]
    fn normal_multiline_and_final_unterminated_event_preserve_terminal_signal() {
        let input = b": heartbeat\r\n\r\nevent: result\r\ndata: {\r\ndata: \"uri\":\"tos://safe\"}\r\n\r\nevent: done\ndata: {}";
        let mut observed = Vec::new();
        assert_eq!(consume(&input[..], deadline(), |name, data| { observed.push((name.to_owned(), data.to_owned())); Ok(name == "done") }), Ok(()));
        assert_eq!(observed.len(), 2); assert_eq!(observed[0].1, "{\n\"uri\":\"tos://safe\"}");
    }
    #[test]
    fn eof_or_partial_result_never_invents_completion() {
        for input in [b"".as_slice(), b": heartbeat\n\n", b"event: result\ndata: {}\n\n"] {
            assert_eq!(consume(input, deadline(), |_, _| Ok(false)), Err(Fault::Incomplete));
        }
    }
    #[test]
    fn protocol_and_length_faults_have_closed_nonfinancial_diagnostics() {
        assert_eq!(consume(b"event: result\ndata: broken\n\n".as_slice(), deadline(), |_, _| Err("token=private".into())), Err(Fault::InvalidEvent));
        assert_eq!(consume(b"data: \xff\n".as_slice(), deadline(), |_, _| Ok(false)), Err(Fault::InvalidEvent));
        let huge = vec![b'x'; MAX_EVENT_BYTES + 5];
        assert_eq!(consume(huge.as_slice(), deadline(), |_, _| Ok(false)), Err(Fault::TooLarge));
        let multiline = "data: x\n".repeat(MAX_EVENT_BYTES / 8 + 1);
        assert_eq!(consume(multiline.as_bytes(), deadline(), |_, _| Ok(false)), Err(Fault::TooLarge));
        for fault in [Fault::Read, Fault::Timeout, Fault::Incomplete, Fault::InvalidEvent, Fault::TooLarge] {
            let diagnostic = fault.diagnostic();
            assert!(!diagnostic.outcome_known); assert!(diagnostic.upstream_error.is_none());
            assert_eq!(diagnostic.stage, "video_stream"); assert!(!serde_json::to_string(&diagnostic).unwrap().contains("private"));
        }
    }
    #[test]
    fn terminal_event_stops_before_any_trailing_read() {
        assert_eq!(consume(b"event: error\ndata: {}\n\ndata: private\n\n".as_slice(), deadline(), |name, _| Ok(name == "error")), Ok(()));
        assert_eq!(consume(b"data: {}\n\n".as_slice(), std::time::Instant::now(), |_, _| panic!("expired")), Err(Fault::Timeout));
    }
    #[test]
    fn real_http_total_deadline_bounds_heartbeats_partial_lines_and_silence() {
        use std::{net::TcpListener, io::Write, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
        for payload in [b": heartbeat\n\n".as_slice(), b"data: unfinished", b""] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/stream", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false)); let ended = stop.clone();
            let bytes = payload.to_vec();
            let server = std::thread::spawn(move || {
                let (mut connection, _) = listener.accept().unwrap();
                connection.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") { let mut byte = [0]; if connection.read_exact(&mut byte).is_err() { return; } request.push(byte[0]); }
                connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                let limit = Instant::now() + Duration::from_secs(2);
                while !ended.load(Ordering::Relaxed) && Instant::now() < limit {
                    if connection.write_all(&bytes).is_err() { break; }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            let start = Instant::now(); let timeout = Duration::from_millis(150);
            let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
            let response = post(&agent, &url, timeout).call().unwrap();
            assert_eq!(response.status(), 200);
            let outcome = consume(BufReader::new(response.into_reader()), start + timeout, |_, _| panic!("no complete data event"));
            stop.store(true, Ordering::Relaxed); server.join().unwrap();
            assert_eq!(outcome, Err(Fault::Timeout)); assert!(start.elapsed() < Duration::from_secs(1));
        }
    }

}

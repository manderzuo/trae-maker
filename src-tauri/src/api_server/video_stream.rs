//! Video-only SSE consumer. Stream faults never prove zero charge.
use std::io::BufRead;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fault { Read, Timeout, Incomplete, InvalidEvent, TooLarge }

pub(super) const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(13 * 60);
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
        super::bridge_diagnostics::Diagnostic { code: code.into(), stage: "video_stream".into(), outcome_known: false, upstream_error: None }
    }
}
pub(super) fn post(agent: &ureq::Agent, url: &str, timeout: std::time::Duration) -> ureq::Request {
    // ureq's deadline is carried into into_reader(), including each blocking
    // socket read. Heartbeats cannot restart it. Only video generation uses it.
    super::video_protocol::post(agent, url).timeout(timeout)
}
pub(super) fn consume(mut reader: impl BufRead, deadline: std::time::Instant,
    mut event: impl FnMut(&str, &str) -> Result<bool, String>) -> Result<(), Fault> {
    use std::io::Read;
    let mut name = String::new();
    let mut data = String::new();
    let mut frame_bytes = 0usize;
    loop {
        if std::time::Instant::now() >= deadline { return Err(Fault::Timeout); }
        let mut bytes = Vec::new();
        // Bound a line before allocating it in full, including a never-ended line.
        let size = reader.by_ref().take((MAX_EVENT_BYTES + 1) as u64).read_until(b'\n', &mut bytes)
            .map_err(|e| if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) { Fault::Timeout } else { Fault::Read })?;
        if std::time::Instant::now() >= deadline { return Err(Fault::Timeout); }
        if size == 0 {
            if !data.is_empty() && event(&name, &data).map_err(|_| Fault::InvalidEvent)? { return Ok(()); }
            return Err(Fault::Incomplete);
        }
        frame_bytes = frame_bytes.checked_add(size).ok_or(Fault::TooLarge)?;
        if frame_bytes > MAX_EVENT_BYTES { return Err(Fault::TooLarge); }
        let line = std::str::from_utf8(&bytes).map_err(|_| Fault::InvalidEvent)?.trim_end_matches(['\r','\n']);
        if line.trim().is_empty() {
            if !data.is_empty() && event(&name, &data).map_err(|_| Fault::InvalidEvent)? { return Ok(()); }
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

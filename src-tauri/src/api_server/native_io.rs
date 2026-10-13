//! Network budget shared by all stages of one native resource operation.
use serde_json::Value;
use std::time::{Duration, Instant};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, OnceLock,
    },
};

pub(super) const PREPARATION_LIMIT: Duration = Duration::from_secs(110);
pub(super) const ARTIFACT_LIMIT: Duration = Duration::from_secs(110);
const MAX_REPLY: u64 = 256 * 1024;
const DNS_WORKERS: usize = 4;

// OS DNS cannot be interrupted by ureq. Only the read-only resolver is detached:
// its slot stays owned until it really exits, and a late answer has no socket,
// request body, credentials or continuation capable of sending an upload.
fn lookup_until(
    budget: Budget,
    slots: Arc<AtomicUsize>,
    lookup: impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
) -> io::Result<Vec<SocketAddr>> {
    let timeout = budget
        .remaining()
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native DNS deadline"))?;
    slots
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < DNS_WORKERS).then_some(n + 1)
        })
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "native DNS busy"))?;
    struct Slot(Arc<AtomicUsize>);
    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let slot = Slot(slots);
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("native-dns".into())
        .spawn(move || {
            let _slot = slot;
            let _ = tx.send(lookup());
        })?;
    rx.recv_timeout(timeout)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native DNS deadline"))?
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    deadline: Instant,
}
#[derive(Debug)]
struct DeadlineIo {
    inner: Box<dyn ureq::ReadWrite>,
    budget: Budget,
}
impl DeadlineIo {
    fn refresh(&self) -> io::Result<()> {
        let remaining = self
            .budget
            .remaining()
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native I/O deadline"))?;
        if let Some(socket) = self.inner.socket() {
            socket.set_read_timeout(Some(remaining))?;
            socket.set_write_timeout(Some(remaining))?;
        }
        Ok(())
    }
}
impl ureq::ReadWrite for DeadlineIo {
    fn socket(&self) -> Option<&std::net::TcpStream> {
        self.inner.socket()
    }
}
impl Read for DeadlineIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.refresh()?;
        self.inner.read(buf)
    }
}
impl Write for DeadlineIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.refresh()?;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.refresh()?;
        self.inner.flush()
    }
}
struct DeadlineTls {
    budget: Budget,
}
impl ureq::TlsConnector for DeadlineTls {
    fn connect(
        &self,
        name: &str,
        stream: Box<dyn ureq::ReadWrite>,
    ) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        static CONFIG: OnceLock<Arc<ureq::rustls::ClientConfig>> = OnceLock::new();
        // This is ureq 2.12's default configuration, with the same roots and
        // verification. The wrapper is below TLS, so handshake and every raw
        // write observe the deadline, even when a slow peer keeps accepting data.
        let config = CONFIG.get_or_init(|| {
            Arc::new(
                ureq::rustls::ClientConfig::builder_with_provider(
                    ureq::rustls::crypto::ring::default_provider().into(),
                )
                .with_protocol_versions(&[
                    &ureq::rustls::version::TLS12,
                    &ureq::rustls::version::TLS13,
                ])
                .expect("ring supports TLS 1.2/1.3")
                .with_root_certificates(ureq::rustls::RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                })
                .with_no_client_auth(),
            )
        });
        ureq::TlsConnector::connect(
            config,
            name,
            Box::new(DeadlineIo {
                inner: stream,
                budget: self.budget,
            }),
        )
    }
}
impl Budget {
    pub(super) fn new(limit: Duration) -> Self {
        Self {
            deadline: Instant::now() + limit,
        }
    }
    pub(super) fn capped(self, limit: Duration) -> Self {
        Self {
            deadline: self.deadline.min(Instant::now() + limit),
        }
    }
    pub(super) fn remaining(self) -> Result<Duration, String> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or("native_io_timeout".into())
    }
    pub(super) fn request(self, method: &str, url: &str) -> Result<ureq::Request, String> {
        Ok(self
            .agent()?
            .request(method, url)
            .timeout(self.remaining()?))
    }
    pub(super) fn agent(self) -> Result<ureq::Agent, String> {
        let remaining = self.remaining()?;
        Ok(ureq::AgentBuilder::new()
            .timeout(remaining)
            .timeout_connect(remaining.min(Duration::from_secs(10)))
            .tls_connector(Arc::new(DeadlineTls { budget: self }))
            .redirects(0)
            .try_proxy_from_env(false)
            .resolver(move |host: &str| {
                static SLOTS: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
                // Literal addresses need no worker (also used by offline tests).
                if let Ok(address) = host.parse::<SocketAddr>() {
                    return Ok(vec![address]);
                }
                let host = host.to_owned();
                lookup_until(
                    self,
                    SLOTS.get_or_init(|| Arc::new(AtomicUsize::new(0))).clone(),
                    move || host.to_socket_addrs().map(|a| a.collect()),
                )
            })
            .build())
    }
}
pub(super) fn reply(response: ureq::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_REPLY + 1)
        .read_to_end(&mut body)
        .map_err(|_| "native_io_read_failed")?;
    if body.len() as u64 > MAX_REPLY {
        return Err("native_io_response_too_large".into());
    }
    Ok(body)
}
pub(super) fn json(response: ureq::Response) -> Result<Value, String> {
    serde_json::from_slice(&reply(response)?).map_err(|_| "native_io_response_invalid".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    fn response(parts: Vec<Vec<u8>>, pause: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/fixture", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = s.read(&mut request);
            for part in parts {
                if s.write_all(&part).is_err() {
                    break;
                }
                thread::sleep(pause);
            }
        });
        (url, worker)
    }
    #[test]
    fn native_io_total_budget_stops_trickling_plan_commit_and_lookup() {
        for method in ["POST", "PUT", "GET"] {
            let (url, worker) = response(
                vec![
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n".to_vec(),
                    b"{".to_vec(),
                    b"\"a\"".to_vec(),
                    b":1}".to_vec(),
                ],
                Duration::from_millis(80),
            );
            let start = Instant::now();
            let budget = Budget::new(Duration::from_millis(100));
            let result = budget
                .request(method, &url)
                .and_then(|r| r.call().map_err(|_| "request".into()))
                .and_then(json);
            let elapsed = start.elapsed();
            worker.join().unwrap();
            assert!(
                result.is_err(),
                "{method}: trickling response exceeded the complete operation deadline"
            );
            assert!(
                elapsed < Duration::from_millis(230),
                "{method}: bounded response took {elapsed:?}"
            );
        }
    }
    #[test]
    fn native_io_rejects_oversize_json_before_deserializing() {
        let body = format!("{{\"value\":\"{}\"}}", "x".repeat(256 * 1024));
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(body.as_bytes());
        let (url, worker) = response(vec![wire], Duration::ZERO);
        let value = json(
            Budget::new(Duration::from_secs(2))
                .request("GET", &url)
                .unwrap()
                .call()
                .unwrap(),
        );
        worker.join().unwrap();
        assert!(
            value.is_err(),
            "native metadata must have a finite body limit"
        );
    }
    #[test]
    fn native_io_stalled_dns_retains_slot_but_cannot_continue_request() {
        let slots = Arc::new(AtomicUsize::new(0));
        let (send, wait) = mpsc::channel();
        let result = lookup_until(
            Budget::new(Duration::from_millis(20)),
            slots.clone(),
            move || {
                wait.recv().unwrap();
                Ok(vec!["127.0.0.1:1".parse().unwrap()])
            },
        );
        assert!(result.is_err());
        assert_eq!(slots.load(Ordering::Acquire), 1);
        let full = Arc::new(AtomicUsize::new(4));
        assert!(
            lookup_until(Budget::new(Duration::from_secs(1)), full, || panic!(
                "DNS capacity must be checked before spawn"
            ))
            .is_err()
        );
        send.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while slots.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    #[cfg(windows)]
    #[test]
    fn native_io_upload_backpressure_cannot_reset_write_deadline() {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            setsockopt, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF,
        };
        fn small_buffer(stream: &std::net::TcpStream, option: i32) {
            let size = 1024i32;
            assert_eq!(
                unsafe {
                    setsockopt(
                        stream.as_raw_socket() as _,
                        SOL_SOCKET,
                        option,
                        (&size as *const i32).cast(),
                        4,
                    )
                },
                0
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            small_buffer(&stream, SO_RCVBUF);
            ready.send(()).unwrap();
            thread::sleep(Duration::from_millis(350));
            drop(stream);
        });
        let socket = std::net::TcpStream::connect(address).unwrap();
        small_buffer(&socket, SO_SNDBUF);
        wait.recv().unwrap();
        let start = Instant::now();
        let mut stream = DeadlineIo {
            inner: Box::new(socket),
            budget: Budget::new(Duration::from_millis(100)),
        };
        let result = (|| {
            for chunk in vec![0; 32 * 1024 * 1024].chunks(16 * 1024) {
                stream.write_all(chunk)?;
            }
            Ok::<_, io::Error>(())
        })();
        let elapsed = start.elapsed();
        drop(stream);
        worker.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(250),
            "upload write blocked beyond total budget: {elapsed:?}, {result:?}"
        );
    }
    #[test]
    fn native_io_repeated_progress_cannot_extend_the_upload_deadline() {
        #[derive(Debug)]
        struct SlowWrites;
        impl Read for SlowWrites {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for SlowWrites {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                thread::sleep(Duration::from_millis(20));
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl ureq::ReadWrite for SlowWrites {
            fn socket(&self) -> Option<&std::net::TcpStream> {
                None
            }
        }
        let mut stream = DeadlineIo {
            inner: Box::new(SlowWrites),
            budget: Budget::new(Duration::from_millis(50)),
        };
        let start = Instant::now();
        let result = stream.write_all(&[0; 10]);
        assert!(
            result.is_err(),
            "steady write progress must not renew the operation deadline"
        );
        assert!(start.elapsed() < Duration::from_millis(150));
    }
}

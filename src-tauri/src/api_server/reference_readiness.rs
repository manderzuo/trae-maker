//! Read-only native reference probes before any paid video dispatch.
//! Resolving a URL does not extend the credentials used internally by generation.
use serde_json::Value;
use std::{io::Read, net::{SocketAddr,ToSocketAddrs}, time::{Duration, Instant}, sync::{Arc, OnceLock, mpsc, atomic::{AtomicUsize, Ordering}}};
use super::{pool::PickedAccount, trae_resource_upload::NativeUploadError};

const PROBE_DEADLINE: Duration = Duration::from_secs(20);
const MAX_REPLY: u64 = 256 * 1024;
const PREFIX_BYTES: u64 = 32;
const MAX_PROBE_WORKERS: usize = 4;
fn unavailable() -> NativeUploadError {
    NativeUploadError::new("reference_resource_unavailable", false)
}
fn remaining(start: Instant) -> Result<Duration, NativeUploadError> {
    PROBE_DEADLINE.checked_sub(start.elapsed()).filter(|d| !d.is_zero()).ok_or_else(unavailable)
}
fn public_addresses(addresses:Vec<SocketAddr>) -> std::io::Result<Vec<SocketAddr>> {
    if addresses.is_empty() || addresses.iter().any(|a|super::assets::private_or_reserved_host(&a.ip().to_string())) {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,"reference_resource_unavailable"))
    } else {Ok(addresses)}
}
fn probe_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).try_proxy_from_env(false)
        .resolver(|host:&str|public_addresses(host.to_socket_addrs()?.collect())).build()
}
fn resource_url<'a>(reply: &'a Value, uri: &str, now: i64) -> Result<&'a str, NativeUploadError> {
    super::trae_resource_upload::validate_acknowledgement(reply)?;
    let data = reply.get("data").unwrap_or(reply);
    let raw = data["url_map"][uri].as_str().ok_or_else(unavailable)?;
    let url = url::Url::parse(raw).map_err(|_| unavailable())?;
    let host = url.host_str().ok_or_else(unavailable)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
        || url.port().is_some() || url.fragment().is_some()
        || ![".byteimg.com", ".pstatp.com", ".ibytedtos.com", ".volces.com", ".bytecdn.cn", ".bytedance.net", ".bytedanceapi.com"].iter().any(|suffix| host.ends_with(suffix)) {
        return Err(unavailable());
    }
    for (key, expiry) in url.query_pairs() {
        if key == "x-expires" || key == "Expires" {
            if expiry.parse::<i64>().ok().filter(|e| *e > now.saturating_add(30)).is_none() {
                return Err(unavailable());
            }
        }
    }
    Ok(raw)
}
fn media_prefix(expected: &str, status: u16, content_type: Option<&str>, bytes: &[u8]) -> Result<(), NativeUploadError> {
    if !matches!(status, 200 | 206) {return Err(unavailable());}
    let detected = super::assets::detect_format(bytes).map(|p| p.0).ok_or_else(unavailable)?;
    let family = if expected.starts_with("image/") {"image/"} else {"video/"};
    let declared = content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if !detected.starts_with(family) || !(declared.starts_with(family) || declared == "application/octet-stream") {
        return Err(unavailable());
    }
    Ok(())
}
fn bounded_probe(start:Instant, timeout:Duration, slots:Arc<AtomicUsize>,
    operation:impl FnOnce()->Result<(),NativeUploadError>+Send+'static) -> Result<(),NativeUploadError> {
    slots.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<MAX_PROBE_WORKERS).then_some(n+1))
        .map_err(|_|unavailable())?;
    struct Slot(Arc<AtomicUsize>);
    impl Drop for Slot {fn drop(&mut self){self.0.fetch_sub(1,Ordering::AcqRel);}}
    let slot=Slot(slots);
    let (send,receive)=mpsc::sync_channel(1);
    std::thread::Builder::new().name("reference-probe".into()).spawn(move||{
        let _slot=slot;let _=send.send(operation());
    }).map_err(|_|unavailable())?;
    // SDK DNS may outlive its network timeout. The observer still closes at
    // the deadline, while the stalled read-only worker keeps its bounded slot.
    let wait=timeout.checked_sub(start.elapsed()).ok_or_else(unavailable)?;
    receive.recv_timeout(wait).map_err(|_|unavailable())?
}
pub(super) fn check(account: &PickedAccount, body: &Value) -> Result<(), NativeUploadError> {
    check_with_budget(account,body,super::native_io::Budget::new(PROBE_DEADLINE))
}
pub(super) fn check_with_budget(account:&PickedAccount,body:&Value,budget:super::native_io::Budget)->Result<(),NativeUploadError> {
    if !["image_urls","video_urls"].iter().any(|key|body[key].as_array().is_some_and(|a|!a.is_empty())) {return Ok(());}
    static SLOTS:OnceLock<Arc<AtomicUsize>>=OnceLock::new();
    let start=Instant::now();let account=account.clone();let body=body.clone();
    let budget=budget.capped(PROBE_DEADLINE);let timeout=budget.remaining().map_err(|_|unavailable())?;
    bounded_probe(start,timeout,SLOTS.get_or_init(||Arc::new(AtomicUsize::new(0))).clone(),move||check_until(&account,&body,budget))
}
fn check_until(account: &PickedAccount, body: &Value, budget:super::native_io::Budget) -> Result<(), NativeUploadError> {
    let mut references = Vec::new();
    for (key, kind) in [("image_urls", "image/"), ("video_urls", "video/")] {
        for uri in body[key].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !references.contains(&(uri, kind)) {references.push((uri, kind));}
        }
    }
    if references.is_empty() {return Ok(());}
    let agent=probe_agent();
    let response = super::trae_resource_upload::common_request_with_agent(account, "/api/ide/v1/get_resource_url",&agent)
        .timeout(budget.remaining().map_err(|_|unavailable())?)
        .send_json(serde_json::json!({"uri_list":references.iter().map(|r|r.0).collect::<Vec<_>>() }))
        .map_err(|_| unavailable())?;
    if response.status()!=200 {return Err(unavailable());}
    let mut reply = Vec::new();
    response.into_reader().take(MAX_REPLY + 1).read_to_end(&mut reply).map_err(|_| unavailable())?;
    if reply.len() as u64 > MAX_REPLY {return Err(unavailable());}
    let reply: Value = serde_json::from_slice(&reply).map_err(|_| unavailable())?;
    // No account credentials or authentication headers are sent to the CDN.
    for (uri, kind) in references {
        let raw = resource_url(&reply, uri, chrono::Utc::now().timestamp()).map_err(|_| unavailable())?;
        let response = agent.get(raw).timeout(budget.remaining().map_err(|_|unavailable())?).set("Range", "bytes=0-31")
            .call().map_err(|_| unavailable())?;
        let status = response.status();
        let content_type = response.header("content-type").map(str::to_string);
        let mut prefix = Vec::new();
        response.into_reader().take(PREFIX_BYTES).read_to_end(&mut prefix).map_err(|_| unavailable())?;
        media_prefix(kind, status, content_type.as_deref(), &prefix)?;
        budget.remaining().map_err(|_|unavailable())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_preflight_never_uses_expired_private_redirect_or_credential_urls() {
        for raw in ["https://127.0.0.1/secret", "http://p1.byteimg.com/a", "https://u:p@p1.byteimg.com/a",
            "https://p1.byteimg.com:444/a", "https://p1.byteimg.com.attacker.test/a", "https://p-copilot.byted.org/a",
            "https://p1.byteimg.com/a?x-expires=1000&x-signature=private", "https://p1.byteimg.com/a?x-expires=broken"] {
            let reply=serde_json::json!({"url_map":{"fixture":raw}});
            let error=resource_url(&reply,"fixture",1000).unwrap_err();
            assert_eq!(error.message,"reference_resource_unavailable");
        }
        let raw="https://p1.byteimg.com/a?x-expires=1600&x-signature=fixture";
        assert_eq!(resource_url(&serde_json::json!({"data":{"url_map":{"fixture":raw}}}),"fixture",1000).unwrap(),raw);
    }
    #[test]
    fn resource_preflight_requires_readable_media_not_an_html_error_with_http_200() {
        assert!(media_prefix("image/",200,Some("text/html"),b"<html>expired credential</html>").is_err());
        assert!(media_prefix("image/",403,Some("image/png"),b"\x89PNG\r\n\x1a\n").is_err());
        assert!(media_prefix("video/",200,Some("image/png"),b"\x89PNG\r\n\x1a\n").is_err());
        assert!(media_prefix("image/",206,Some("image/png"),b"\x89PNG\r\n\x1a\n").is_ok());
        assert!(media_prefix("video/",200,Some("application/octet-stream"),b"\0\0\0\x18ftypisom").is_ok());
    }
    #[test]
    fn resource_preflight_has_one_deadline_for_all_references() {
        assert!(remaining(Instant::now()-Duration::from_secs(21)).is_err());
        assert!(remaining(Instant::now()).unwrap()<=PROBE_DEADLINE);
    }
    #[test]
    fn resource_preflight_dns_stall_closes_observer_and_keeps_a_bounded_worker_slot() {
        let slots=Arc::new(AtomicUsize::new(0));let (send,receive)=mpsc::channel();
        let result=bounded_probe(Instant::now(),Duration::from_millis(20),slots.clone(),move||{let _=receive.recv();Ok(())});
        assert!(result.is_err());assert_eq!(slots.load(Ordering::Acquire),1);
        send.send(()).unwrap();
        let limit=Instant::now()+Duration::from_secs(1);
        while slots.load(Ordering::Acquire)!=0 {assert!(Instant::now()<limit);std::thread::yield_now();}
        let occupied=Arc::new(AtomicUsize::new(MAX_PROBE_WORKERS));
        assert!(bounded_probe(Instant::now(),Duration::from_millis(20),occupied.clone(),||panic!("busy gate must not start a worker")).is_err());
        assert_eq!(occupied.load(Ordering::Acquire),MAX_PROBE_WORKERS);
    }
    #[test]
    fn resource_preflight_dns_never_connects_to_private_or_mixed_address_sets() {
        for ip in ["127.0.0.1:443","10.0.0.1:443","169.254.169.254:443","[::1]:443","[::ffff:127.0.0.1]:443"] {
            assert!(public_addresses(vec![ip.parse().unwrap()]).is_err());
            assert!(public_addresses(vec!["93.184.216.34:443".parse().unwrap(),ip.parse().unwrap()]).is_err());
        }
        assert!(public_addresses(vec!["93.184.216.34:443".parse().unwrap()]).is_ok());
    }
    #[cfg(windows)]
    #[test]
    #[ignore="explicit read-only native check; credentials supplied in memory on stdin; no generation"]
    fn live_resource_preflight_read_only() {
        assert_eq!(std::env::var("BRIDGE_REFERENCE_READBACK_ACK").as_deref(),Ok("1"));
        let mut input=String::new();std::io::stdin().take(32*1024).read_to_string(&mut input).unwrap();
        let input:Value=serde_json::from_str(&input).unwrap();
        let account=PickedAccount {uid:String::new(),jwt:input["jwt"].as_str().unwrap().into(),
            device_id:input["device"].as_str().unwrap().into(),machine_id:input["device"].as_str().unwrap().into(),
            domain:String::new(),enterprise_id:String::new(),global_region:false};
        check(&account,&serde_json::json!({"image_urls":[input["resource"]]})).unwrap();
    }
}

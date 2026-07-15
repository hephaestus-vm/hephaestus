//! Transparent MMDS over a vmnet packet interface.
//!
//! The responder claims `169.254.169.254` at Ethernet layer 2, answers guest
//! ARP, and implements the small TCP/HTTP subset needed for Firecracker-style
//! MMDS GET requests. This avoids modifying host interfaces, requiring root,
//! or depending on guest agents and routes.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use http_body_util::Full;
#[cfg(test)]
use hyper::body::Bytes;
#[cfg(test)]
use hyper::{Method, Request, Response, StatusCode};
use serde_json::Value;

#[cfg(test)]
use crate::backend::VzBackend;
use crate::backend::VzVmHandle;
#[cfg(test)]
use hephaestus_fc_api::VmmBackend;
use hephaestus_fc_api::VmmBackendError;
#[cfg(test)]
use tokio::sync::Mutex;

/// Link-local address Firecracker guests use for MMDS lookups.
pub const MMDS_LINK_LOCAL_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
/// Default port Firecracker exposes MMDS on.
pub const MMDS_LINK_LOCAL_PORT: u16 = 80;

const SERVICE_MAC: [u8; 6] = [0x02, 0x48, 0x50, 0x4d, 0x44, 0x53];
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const TCP_FLAG_FIN: u8 = 0x01;
const TCP_FLAG_SYN: u8 = 0x02;
const TCP_FLAG_PSH: u8 = 0x08;
const TCP_FLAG_ACK: u8 = 0x10;
const MAX_TCP_PAYLOAD: usize = 1200;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ClientKey {
    mac: [u8; 6],
    ip: [u8; 4],
    port: u16,
}

struct Connection {
    client_next: u32,
    server_first: u32,
    request: Vec<u8>,
    response: Option<Vec<u8>>,
    last_seen: Instant,
}

/// Background raw-packet MMDS service attached to the VM's vmnet network.
#[derive(Debug)]
pub(crate) struct VmnetMmdsResponder {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VmnetMmdsResponder {
    pub(crate) fn start(
        vm: Arc<dyn VzVmHandle>,
        mmds: Arc<RwLock<Value>>,
    ) -> Result<Self, VmmBackendError> {
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = std::thread::Builder::new()
            .name("hephaestus-vmnet-mmds".into())
            .spawn(move || {
                let mut responder = PacketResponder::new(mmds);
                let mut frame = vec![0u8; 65_536];
                while thread_running.load(Ordering::Acquire) {
                    match vm.vmnet_read(&mut frame) {
                        Ok(0) => std::thread::sleep(Duration::from_millis(1)),
                        Ok(len) => {
                            for reply in responder.process(&frame[..len]) {
                                if let Err(err) = vm.vmnet_write(&reply) {
                                    eprintln!(
                                        "hephaestus-firecracker: vmnet MMDS write failed ({err})"
                                    );
                                    thread_running.store(false, Ordering::Release);
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!("hephaestus-firecracker: vmnet MMDS read failed ({err})");
                            thread_running.store(false, Ordering::Release);
                        }
                    }
                }
            })
            .map_err(|err| VmmBackendError::Internal(format!("spawn vmnet MMDS: {err}")))?;
        eprintln!(
            "hephaestus-firecracker: transparent MMDS active at http://{}:{}/",
            MMDS_LINK_LOCAL_ADDR, MMDS_LINK_LOCAL_PORT
        );
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }
}

impl Drop for VmnetMmdsResponder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct PacketResponder {
    mmds: Arc<RwLock<Value>>,
    connections: HashMap<ClientKey, Connection>,
}

impl PacketResponder {
    fn new(mmds: Arc<RwLock<Value>>) -> Self {
        Self {
            mmds,
            connections: HashMap::new(),
        }
    }

    fn process(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        if frame.len() < 14 {
            return Vec::new();
        }
        match read_u16(frame, 12) {
            Some(ETHERTYPE_ARP) => self.process_arp(frame).into_iter().collect(),
            Some(ETHERTYPE_IPV4) => self.process_ipv4(frame),
            _ => Vec::new(),
        }
    }

    fn process_arp(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 42
            || read_u16(frame, 14) != Some(1)
            || read_u16(frame, 16) != Some(ETHERTYPE_IPV4)
            || frame[18] != 6
            || frame[19] != 4
            || read_u16(frame, 20) != Some(1)
            || frame[38..42] != MMDS_LINK_LOCAL_ADDR.octets()
        {
            return None;
        }
        let mut client_mac = [0u8; 6];
        client_mac.copy_from_slice(&frame[22..28]);
        let mut client_ip = [0u8; 4];
        client_ip.copy_from_slice(&frame[28..32]);

        let mut reply = vec![0u8; 42];
        reply[0..6].copy_from_slice(&client_mac);
        reply[6..12].copy_from_slice(&SERVICE_MAC);
        write_u16(&mut reply, 12, ETHERTYPE_ARP);
        write_u16(&mut reply, 14, 1);
        write_u16(&mut reply, 16, ETHERTYPE_IPV4);
        reply[18] = 6;
        reply[19] = 4;
        write_u16(&mut reply, 20, 2);
        reply[22..28].copy_from_slice(&SERVICE_MAC);
        reply[28..32].copy_from_slice(&MMDS_LINK_LOCAL_ADDR.octets());
        reply[32..38].copy_from_slice(&client_mac);
        reply[38..42].copy_from_slice(&client_ip);
        Some(reply)
    }

    fn process_ipv4(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        if frame.len() < 54 || frame[14] >> 4 != 4 || frame[23] != 6 {
            return Vec::new();
        }
        let ip_header_len = usize::from(frame[14] & 0x0f) * 4;
        if ip_header_len < 20 || frame.len() < 14 + ip_header_len + 20 {
            return Vec::new();
        }
        let total_len = usize::from(read_u16(frame, 16).unwrap_or(0));
        if total_len < ip_header_len + 20 || frame.len() < 14 + total_len {
            return Vec::new();
        }
        let mut client_ip = [0u8; 4];
        client_ip.copy_from_slice(&frame[26..30]);
        if frame[30..34] != MMDS_LINK_LOCAL_ADDR.octets() {
            return Vec::new();
        }
        let tcp = 14 + ip_header_len;
        let client_port = read_u16(frame, tcp).unwrap_or(0);
        if read_u16(frame, tcp + 2) != Some(MMDS_LINK_LOCAL_PORT) {
            return Vec::new();
        }
        let tcp_header_len = usize::from(frame[tcp + 12] >> 4) * 4;
        if tcp_header_len < 20 || tcp + tcp_header_len > 14 + total_len {
            return Vec::new();
        }
        let seq = read_u32(frame, tcp + 4).unwrap_or(0);
        let flags = frame[tcp + 13];
        let payload = &frame[tcp + tcp_header_len..14 + total_len];
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[6..12]);
        let key = ClientKey {
            mac,
            ip: client_ip,
            port: client_port,
        };

        self.connections
            .retain(|_, conn| conn.last_seen.elapsed() < Duration::from_secs(30));

        if flags & TCP_FLAG_SYN != 0 {
            if !self.connections.contains_key(&key) && self.connections.len() >= 1024 {
                return Vec::new();
            }
            let server_first = initial_sequence(&key);
            self.connections.insert(
                key,
                Connection {
                    client_next: seq.wrapping_add(1),
                    server_first,
                    request: Vec::new(),
                    response: None,
                    last_seen: Instant::now(),
                },
            );
            return vec![build_tcp_frame(
                key,
                server_first,
                seq.wrapping_add(1),
                TCP_FLAG_SYN | TCP_FLAG_ACK,
                &[],
            )];
        }

        let Some(conn) = self.connections.get_mut(&key) else {
            return Vec::new();
        };
        conn.last_seen = Instant::now();

        if !payload.is_empty() && seq == conn.client_next {
            if conn.request.len().saturating_add(payload.len()) > 64 * 1024 {
                return Vec::new();
            }
            conn.request.extend_from_slice(payload);
            conn.client_next = conn.client_next.wrapping_add(len_u32(payload.len()));
        }
        if flags & TCP_FLAG_FIN != 0 && seq.wrapping_add(len_u32(payload.len())) == conn.client_next
        {
            conn.client_next = conn.client_next.wrapping_add(1);
        }

        if let Some(response) = conn.response.as_ref() {
            // A payload here is a retransmitted HTTP request, so replay the
            // deterministic response. A pure ACK is the guest acknowledging
            // our response and must not trigger an endless replay loop.
            if !payload.is_empty() {
                return response_frames(
                    key,
                    conn.server_first.wrapping_add(1),
                    conn.client_next,
                    response,
                );
            }
            if flags & TCP_FLAG_FIN != 0 {
                return vec![build_tcp_frame(
                    key,
                    conn.server_first
                        .wrapping_add(1)
                        .wrapping_add(len_u32(response.len()))
                        .wrapping_add(1),
                    conn.client_next,
                    TCP_FLAG_ACK,
                    &[],
                )];
            }
            return Vec::new();
        }

        if conn.request.windows(4).any(|window| window == b"\r\n\r\n") {
            let document = self
                .mmds
                .read()
                .map(|value| value.clone())
                .unwrap_or_else(|_| Value::Null);
            let response = render_raw_http(&conn.request, &document);
            let frames = response_frames(
                key,
                conn.server_first.wrapping_add(1),
                conn.client_next,
                &response,
            );
            conn.response = Some(response);
            frames
        } else {
            vec![build_tcp_frame(
                key,
                conn.server_first.wrapping_add(1),
                conn.client_next,
                TCP_FLAG_ACK,
                &[],
            )]
        }
    }
}

fn response_frames(key: ClientKey, first_seq: u32, ack: u32, response: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = response.chunks(MAX_TCP_PAYLOAD).collect();
    let mut frames = Vec::with_capacity(chunks.len());
    let mut seq = first_seq;
    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == chunks.len();
        let flags = TCP_FLAG_ACK | TCP_FLAG_PSH | if last { TCP_FLAG_FIN } else { 0 };
        frames.push(build_tcp_frame(key, seq, ack, flags, chunk));
        seq = seq.wrapping_add(len_u32(chunk.len()));
    }
    frames
}

fn render_raw_http(request: &[u8], root: &Value) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    let first = text.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = sanitize_path(parts.next().unwrap_or("/"));
    let accept_json = text.lines().any(|line| {
        line.to_ascii_lowercase().starts_with("accept:")
            && line.to_ascii_lowercase().contains("application/json")
    });

    let (status, content_type, body) = if method != "GET" {
        (405, "text/plain", b"Method not allowed".to_vec())
    } else {
        match lookup(root, &path) {
            Some(value) if accept_json => (
                200,
                "application/json",
                serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()),
            ),
            Some(Value::String(value)) => (200, "text/plain", value.into_bytes()),
            Some(value @ Value::Object(_)) => (200, "text/plain", format_imds(&value).into_bytes()),
            Some(value) => (
                200,
                "application/json",
                serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()),
            ),
            None => (
                404,
                "text/plain",
                format!("The MMDS resource does not exist: {path}").into_bytes(),
            ),
        }
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn build_tcp_frame(key: ClientKey, seq: u32, ack: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    let ip_len = 20 + 20 + payload.len();
    let mut frame = vec![0u8; 14 + ip_len];
    frame[0..6].copy_from_slice(&key.mac);
    frame[6..12].copy_from_slice(&SERVICE_MAC);
    write_u16(&mut frame, 12, ETHERTYPE_IPV4);

    let ip = 14;
    frame[ip] = 0x45;
    write_u16(&mut frame, ip + 2, len_u16(ip_len));
    write_u16(&mut frame, ip + 6, 0x4000);
    frame[ip + 8] = 64;
    frame[ip + 9] = 6;
    frame[ip + 12..ip + 16].copy_from_slice(&MMDS_LINK_LOCAL_ADDR.octets());
    frame[ip + 16..ip + 20].copy_from_slice(&key.ip);
    let ip_checksum = checksum(&frame[ip..ip + 20]);
    write_u16(&mut frame, ip + 10, ip_checksum);

    let tcp = ip + 20;
    write_u16(&mut frame, tcp, MMDS_LINK_LOCAL_PORT);
    write_u16(&mut frame, tcp + 2, key.port);
    write_u32(&mut frame, tcp + 4, seq);
    write_u32(&mut frame, tcp + 8, ack);
    frame[tcp + 12] = 5 << 4;
    frame[tcp + 13] = flags;
    write_u16(&mut frame, tcp + 14, 64_240);
    frame[tcp + 20..].copy_from_slice(payload);

    let tcp_len = 20 + payload.len();
    let mut pseudo = Vec::with_capacity(12 + tcp_len + 1);
    pseudo.extend_from_slice(&MMDS_LINK_LOCAL_ADDR.octets());
    pseudo.extend_from_slice(&key.ip);
    pseudo.push(0);
    pseudo.push(6);
    pseudo.extend_from_slice(&len_u16(tcp_len).to_be_bytes());
    pseudo.extend_from_slice(&frame[tcp..]);
    let tcp_checksum = checksum(&pseudo);
    write_u16(&mut frame, tcp + 16, tcp_checksum);
    frame
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], 0])
        };
        sum = sum.wrapping_add(u32::from(word));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum).expect("folded checksum fits in 16 bits")
}

fn len_u16(len: usize) -> u16 {
    u16::try_from(len).expect("Ethernet frame length fits in 16 bits")
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("Ethernet frame length fits in 32 bits")
}

fn initial_sequence(key: &ClientKey) -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    now ^ u32::from_be_bytes(key.ip) ^ u32::from(key.port)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
        *bytes.get(offset + 2)?,
        *bytes.get(offset + 3)?,
    ]))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

// Generic over the request body: `handle` only inspects the method, the
// `Accept` header, and the URI path — never the body — so tests can drive it
// with a bodyless `Request` instead of a live `Incoming` stream.
#[cfg(test)]
async fn handle<B>(req: Request<B>, backend: Arc<Mutex<VzBackend>>) -> Response<Full<Bytes>> {
    if req.method() != Method::GET {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let accept_json = req
        .headers()
        .get_all(hyper::header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains("application/json"));
    let mmds = backend.lock().await.get_mmds();
    let path = sanitize_path(req.uri().path());
    match lookup(&mmds, &path) {
        Some(value) if accept_json => json_response(StatusCode::OK, &value),
        Some(Value::String(s)) => text_response(StatusCode::OK, s.as_str()),
        Some(value @ Value::Object(_)) => text_response(StatusCode::OK, &format_imds(&value)),
        Some(other) => json_response(StatusCode::OK, &other),
        None => text_response(
            StatusCode::NOT_FOUND,
            &format!("The MMDS resource does not exist: {path}"),
        ),
    }
}

/// Resolve a Firecracker-style MMDS path (`/latest/meta-data/...`) into
/// the JSON subtree. Mirrors the vsock-side MMDS service's lookup.
fn lookup(root: &Value, path: &str) -> Option<Value> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(root.clone());
    }
    let mut current = root;
    for segment in trimmed.split('/') {
        let key = segment.replace("~1", "/").replace("~0", "~");
        match current {
            Value::Object(map) => {
                current = map.get(&key)?;
            }
            Value::Array(arr) => {
                let idx: usize = key.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

fn sanitize_path(path: &str) -> String {
    let raw = path.split_once('?').map_or(path, |(path, _)| path);
    let mut sanitized = if raw.is_empty() {
        "/".to_string()
    } else {
        raw.to_string()
    };
    while sanitized.contains("//") {
        sanitized = sanitized.replace("//", "/");
    }
    sanitized
}

fn format_imds(value: &Value) -> String {
    let Some(map) = value.as_object() else {
        return String::new();
    };
    let mut keys: Vec<_> = map
        .iter()
        .map(|(key, value)| {
            if value.is_object() {
                format!("{key}/")
            } else {
                key.clone()
            }
        })
        .collect();
    keys.sort();
    keys.join("\n")
}

#[cfg(test)]
fn json_response<T: serde::Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let json = serde_json::to_vec(body).expect("MMDS JSON must serialize");
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

#[cfg(test)]
fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_resolves_object_paths() {
        let mmds = serde_json::json!({
            "latest": {
                "meta-data": {
                    "instance-id": "i-hephaestus",
                    "ami-id": "ami-deadbeef"
                }
            }
        });
        assert_eq!(
            lookup(&mmds, "/latest/meta-data/instance-id"),
            Some(Value::String("i-hephaestus".into()))
        );
        assert_eq!(
            lookup(&mmds, "/latest/meta-data"),
            Some(serde_json::json!({
                "instance-id": "i-hephaestus",
                "ami-id": "ami-deadbeef"
            }))
        );
        assert_eq!(lookup(&mmds, "/latest/nonexistent"), None);
    }

    #[test]
    fn lookup_decodes_json_pointer_escapes() {
        let mmds = serde_json::json!({"a/b": {"tilde~key": "ok"}});
        assert_eq!(
            lookup(&mmds, "/a~1b/tilde~0key"),
            Some(Value::String("ok".into()))
        );
    }

    #[test]
    fn imds_formats_object_keys_like_vsock_mmds() {
        let mmds = serde_json::json!({"z": 1, "a": {"nested": true}});
        assert_eq!(format_imds(&mmds), "a/\nz");
    }

    #[test]
    fn sanitize_path_collapses_slashes_and_strips_query() {
        assert_eq!(sanitize_path("/latest//meta-data?x=1"), "/latest/meta-data");
        assert_eq!(sanitize_path(""), "/");
    }

    #[test]
    fn lookup_handles_root_path() {
        let mmds = serde_json::json!({"a": 1});
        assert_eq!(lookup(&mmds, "/"), Some(serde_json::json!({"a": 1})));
        assert_eq!(lookup(&mmds, ""), Some(serde_json::json!({"a": 1})));
    }

    // ── handle() control-plane coverage ──────────────────────────────────
    // Drives the HTTP semantics independently of the raw Ethernet/TCP layer,
    // so it runs on CI with no vmnet entitlement.

    use http_body_util::BodyExt;

    async fn backend_with(mmds: Value) -> Arc<Mutex<VzBackend>> {
        let mut backend = VzBackend::new("host-mmds-test".into());
        backend.put_mmds(mmds).expect("put_mmds");
        Arc::new(Mutex::new(backend))
    }

    fn get(uri: &str, accept_json: bool) -> Request<()> {
        let mut builder = Request::builder().method(Method::GET).uri(uri);
        if accept_json {
            builder = builder.header(hyper::header::ACCEPT, "application/json");
        }
        builder.body(()).unwrap()
    }

    async fn parts(resp: Response<Full<Bytes>>) -> (StatusCode, String, String) {
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, ctype, String::from_utf8(body.to_vec()).unwrap())
    }

    fn sample() -> Value {
        serde_json::json!({
            "latest": {
                "meta-data": {
                    "instance-id": "i-hephaestus",
                    "placement": {"region": "us-mars-1"}
                }
            }
        })
    }

    #[tokio::test]
    async fn json_accept_returns_subtree_as_json() {
        let backend = backend_with(sample()).await;
        let resp = handle(get("/latest/meta-data", true), backend).await;
        let (status, ctype, body) = parts(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "application/json");
        let got: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(got["instance-id"], "i-hephaestus");
    }

    #[tokio::test]
    async fn plain_accept_lists_object_keys_imds_style() {
        let backend = backend_with(sample()).await;
        let resp = handle(get("/latest/meta-data", false), backend).await;
        let (status, ctype, body) = parts(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "text/plain");
        // Directories get a trailing slash; keys sort; no JSON braces.
        assert_eq!(body, "instance-id\nplacement/");
    }

    #[tokio::test]
    async fn string_leaf_returns_raw_value_as_text() {
        let backend = backend_with(sample()).await;
        let resp = handle(get("/latest/meta-data/instance-id", false), backend).await;
        let (status, ctype, body) = parts(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "text/plain");
        assert_eq!(body, "i-hephaestus");
    }

    #[tokio::test]
    async fn missing_path_is_404() {
        let backend = backend_with(sample()).await;
        let resp = handle(get("/latest/meta-data/nope", false), backend).await;
        let (status, _ctype, body) = parts(resp).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("does not exist"), "body was: {body}");
    }

    #[test]
    fn raw_http_returns_string_leaf() {
        let response = render_raw_http(
            b"GET /latest/meta-data/instance-id HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
            &sample(),
        );
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\r\n\r\ni-hephaestus"));
    }

    #[test]
    fn arp_request_for_metadata_ip_gets_service_reply() {
        let client_mac = [0xaa, 0xfc, 0, 0, 0, 1];
        let client_ip = [192, 168, 64, 2];
        let mut request = vec![0u8; 42];
        request[0..6].fill(0xff);
        request[6..12].copy_from_slice(&client_mac);
        write_u16(&mut request, 12, ETHERTYPE_ARP);
        write_u16(&mut request, 14, 1);
        write_u16(&mut request, 16, ETHERTYPE_IPV4);
        request[18] = 6;
        request[19] = 4;
        write_u16(&mut request, 20, 1);
        request[22..28].copy_from_slice(&client_mac);
        request[28..32].copy_from_slice(&client_ip);
        request[38..42].copy_from_slice(&MMDS_LINK_LOCAL_ADDR.octets());

        let responder = PacketResponder::new(Arc::new(RwLock::new(sample())));
        let reply = responder.process_arp(&request).expect("ARP reply");
        assert_eq!(&reply[0..6], &client_mac);
        assert_eq!(&reply[6..12], &SERVICE_MAC);
        assert_eq!(read_u16(&reply, 20), Some(2));
        assert_eq!(&reply[22..28], &SERVICE_MAC);
        assert_eq!(&reply[28..32], &MMDS_LINK_LOCAL_ADDR.octets());
        assert_eq!(&reply[38..42], &client_ip);
    }

    #[test]
    fn emitted_tcp_frame_has_valid_ip_and_tcp_checksums() {
        let key = ClientKey {
            mac: [0xaa, 0xfc, 0, 0, 0, 1],
            ip: [192, 168, 64, 2],
            port: 49152,
        };
        let frame = build_tcp_frame(key, 10, 20, TCP_FLAG_ACK | TCP_FLAG_PSH, b"hello");
        assert_eq!(checksum(&frame[14..34]), 0);

        let tcp = &frame[34..];
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&MMDS_LINK_LOCAL_ADDR.octets());
        pseudo.extend_from_slice(&key.ip);
        pseudo.push(0);
        pseudo.push(6);
        pseudo.extend_from_slice(&len_u16(tcp.len()).to_be_bytes());
        pseudo.extend_from_slice(tcp);
        assert_eq!(checksum(&pseudo), 0);
    }

    #[tokio::test]
    async fn non_get_is_405() {
        let backend = backend_with(sample()).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/latest/meta-data")
            .body(())
            .unwrap();
        let resp = handle(req, backend).await;
        let (status, _ctype, _body) = parts(resp).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
}

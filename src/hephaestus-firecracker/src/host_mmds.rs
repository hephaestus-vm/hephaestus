//! Host-network MMDS HTTP listener (scaffold).
//!
//! Binds `169.254.169.254:80` on the host and serves the current MMDS JSON
//! document via the same Firecracker-style path-aware semantics as the
//! vsock MMDS service. This is the transparent host-network path that
//! lets arbitrary guest images (without our `hephaestus-agent` link-local
//! shim) fetch metadata from `http://169.254.169.254/` and have it route to
//! the host's listener once `VZVmnetNetworkDeviceAttachment` + the
//! `com.apple.vm.networking` entitlement are in place.
//!
//! Status: scaffold. The HTTP layer is functional; the guest-facing
//! reachability depends on `VZVmnetNetworkDeviceAttachment` being added
//! to the VM's network config (requires `com.apple.vm.networking` and a
//! Developer ID signed binary). Without that entitlement the listener
//! still binds successfully on the host but no guest traffic reaches it.
//! See `docs/guides/networking.md` for the full plan.

use std::net::Ipv4Addr;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::backend::VzBackend;
use hephaestus_fc_api::VmmBackend;

/// Link-local address Firecracker guests use for MMDS lookups.
pub const MMDS_LINK_LOCAL_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
/// Default port Firecracker exposes MMDS on.
pub const MMDS_LINK_LOCAL_PORT: u16 = 80;

/// Start a host-side MMDS HTTP listener on `169.254.169.254:80`.
///
/// Spawns a tokio task that accepts TCP connections and dispatches each
/// to a tiny Firecracker-style MMDS handler. The handler reads the current
/// MMDS JSON out of the shared `VzBackend` and returns JSON subtrees for
/// `Accept: application/json`, IMDS-style plain text otherwise.
///
/// Returns immediately; the listener runs until the process exits. Errors
/// during bind (most likely `EADDRNOTAVAIL` when the link-local address
/// isn't plumbed on a host interface, or `EACCES` when binding port 80 as a
/// non-root user without the entitled binary) are returned to the caller
/// before the task is spawned.
pub async fn spawn(
    backend: Arc<Mutex<VzBackend>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind((MMDS_LINK_LOCAL_ADDR, MMDS_LINK_LOCAL_PORT)).await?;
    eprintln!(
        "hephaestus-firecracker: host MMDS listener on http://{}:{}/",
        MMDS_LINK_LOCAL_ADDR, MMDS_LINK_LOCAL_PORT
    );
    tokio::task::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let backend = backend.clone();
            tokio::task::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: Request<Incoming>| {
                    let backend = backend.clone();
                    async move { Ok::<_, std::convert::Infallible>(handle(req, backend).await) }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok(())
}

// Generic over the request body: `handle` only inspects the method, the
// `Accept` header, and the URI path — never the body — so tests can drive it
// with a bodyless `Request` instead of a live `Incoming` stream.
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

fn json_response<T: serde::Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let json = serde_json::to_vec(body).expect("MMDS JSON must serialize");
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

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
    // Drives the real HTTP dispatch (method check + Accept-based content
    // negotiation + path lookup) without binding the privileged
    // 169.254.169.254:80 socket, so it runs on CI with no entitlement.

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

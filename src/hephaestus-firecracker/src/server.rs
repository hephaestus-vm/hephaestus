//! Request routing + Firecracker-compat error shapes.
//!
//! Hyper 1.x over a tokio UnixStream. One connection handled per task; the
//! backend is behind a `tokio::sync::Mutex` so concurrent connections
//! serialize their backend access the way upstream's single-threaded
//! request injector does.
//!
//! Firecracker error bodies are `{"fault_message": "..."}`. Success on
//! PUT/PATCH is 204 No Content; GET returns 200 with JSON.

use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use hephaestus_fc_api::vmm_config::boot_source::BootSourceConfig;
use hephaestus_fc_api::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig};
use hephaestus_fc_api::vmm_config::logger::LoggerConfig;
use hephaestus_fc_api::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
use hephaestus_fc_api::vmm_config::metrics::MetricsConfig;
use hephaestus_fc_api::vmm_config::net::NetworkInterfaceConfig;
use hephaestus_fc_api::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotConfig};
use hephaestus_fc_api::vmm_config::vm::{UpdatedVm, VmUpdatedState};
use hephaestus_fc_api::{VmmBackend, VmmBackendError};

use crate::backend::VzBackend;

type BoxBody = Full<Bytes>;

pub async fn serve_connection(
    stream: UnixStream,
    backend: Arc<Mutex<VzBackend>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let io = TokioIo::new(stream);
    let service = service_fn(move |req: Request<Incoming>| {
        let backend = backend.clone();
        async move { Ok::<_, std::convert::Infallible>(route(req, backend).await) }
    });
    http1::Builder::new().serve_connection(io, service).await?;
    Ok(())
}

async fn route(req: Request<Incoming>, backend: Arc<Mutex<VzBackend>>) -> Response<BoxBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    match (method, path.as_str()) {
        (Method::GET, "/") => {
            let info = backend.lock().await.instance_info();
            json_response(StatusCode::OK, &info)
        }
        (Method::GET, "/machine-config") => {
            let cfg = backend.lock().await.get_machine_config();
            json_response(StatusCode::OK, &cfg)
        }
        (Method::PUT, "/machine-config") => match parse_body::<MachineConfig>(req).await {
            Ok(cfg) => to_response(backend.lock().await.put_machine_config(cfg)),
            Err(resp) => resp,
        },
        (Method::PATCH, "/machine-config") => match parse_body::<MachineConfigUpdate>(req).await {
            Ok(update) => to_response(backend.lock().await.patch_machine_config(update)),
            Err(resp) => resp,
        },
        (Method::PUT, "/boot-source") => match parse_body::<BootSourceConfig>(req).await {
            Ok(cfg) => to_response(backend.lock().await.configure_boot_source(cfg)),
            Err(resp) => resp,
        },
        (Method::PUT, p) if p.starts_with("/drives/") => {
            let id = p.trim_start_matches("/drives/");
            if id.is_empty() || id.contains('/') {
                return fault(StatusCode::BAD_REQUEST, "invalid drive id");
            }
            match parse_body::<BlockDeviceConfig>(req).await {
                Ok(cfg) if cfg.drive_id != id => fault(
                    StatusCode::BAD_REQUEST,
                    "drive_id in body does not match URL",
                ),
                Ok(cfg) => to_response(backend.lock().await.insert_block_device(cfg)),
                Err(resp) => resp,
            }
        }
        (Method::PATCH, p) if p.starts_with("/drives/") => {
            let id = p.trim_start_matches("/drives/");
            if id.is_empty() || id.contains('/') {
                return fault(StatusCode::BAD_REQUEST, "invalid drive id");
            }
            match parse_body::<BlockDeviceUpdateConfig>(req).await {
                Ok(cfg) if cfg.drive_id != id => fault(
                    StatusCode::BAD_REQUEST,
                    "drive_id in body does not match URL",
                ),
                Ok(cfg) => to_response(backend.lock().await.update_block_device(cfg)),
                Err(resp) => resp,
            }
        }
        (Method::PATCH, "/vm") => match parse_body::<UpdatedVm>(req).await {
            Ok(UpdatedVm {
                state: VmUpdatedState::Paused,
            }) => to_response(backend.lock().await.pause()),
            Ok(UpdatedVm {
                state: VmUpdatedState::Resumed,
            }) => to_response(backend.lock().await.resume()),
            Err(resp) => resp,
        },
        (Method::PUT, "/logger") => match parse_body::<LoggerConfig>(req).await {
            Ok(cfg) => to_response(backend.lock().await.configure_logger(cfg)),
            Err(resp) => resp,
        },
        (Method::PUT, p) if p.starts_with("/network-interfaces/") => {
            let id = p.trim_start_matches("/network-interfaces/");
            if id.is_empty() || id.contains('/') {
                return fault(StatusCode::BAD_REQUEST, "invalid iface id");
            }
            match parse_body::<NetworkInterfaceConfig>(req).await {
                Ok(cfg) if cfg.iface_id != id => fault(
                    StatusCode::BAD_REQUEST,
                    "iface_id in body does not match URL",
                ),
                Ok(cfg) => to_response(backend.lock().await.insert_network_device(cfg)),
                Err(resp) => resp,
            }
        }
        (Method::PATCH, p) if p.starts_with("/network-interfaces/") => {
            let id = p.trim_start_matches("/network-interfaces/");
            if id.is_empty() || id.contains('/') {
                return fault(StatusCode::BAD_REQUEST, "invalid iface id");
            }
            match parse_body::<NetworkInterfaceConfig>(req).await {
                Ok(cfg) if cfg.iface_id != id => fault(
                    StatusCode::BAD_REQUEST,
                    "iface_id in body does not match URL",
                ),
                Ok(cfg) => to_response(backend.lock().await.update_network_device(cfg)),
                Err(resp) => resp,
            }
        }
        (Method::PUT, "/metrics") => match parse_body::<MetricsConfig>(req).await {
            Ok(cfg) => to_response(backend.lock().await.configure_metrics(cfg)),
            Err(resp) => resp,
        },
        (Method::PUT, "/snapshot/create") => match parse_body::<CreateSnapshotParams>(req).await {
            Ok(params) => to_response(backend.lock().await.create_snapshot(params)),
            Err(resp) => resp,
        },
        (Method::PUT, "/snapshot/load") => match parse_body::<LoadSnapshotConfig>(req).await {
            Ok(params) => to_response(backend.lock().await.load_snapshot(params)),
            Err(resp) => resp,
        },
        (Method::PUT, "/actions") => match parse_body::<ActionBody>(req).await {
            Ok(ActionBody {
                action_type: ActionType::InstanceStart,
            }) => to_response(backend.lock().await.start_micro_vm()),
            Ok(ActionBody { action_type }) => fault(
                StatusCode::BAD_REQUEST,
                &format!("action_type `{action_type:?}` is not supported"),
            ),
            Err(resp) => resp,
        },
        (_, p) => fault(
            StatusCode::NOT_FOUND,
            &format!("no handler for {} {}", req.method(), p),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionBody {
    action_type: ActionType,
}

#[derive(Debug, serde::Deserialize)]
enum ActionType {
    InstanceStart,
    FlushMetrics,
    SendCtrlAltDel,
}

async fn parse_body<T: serde::de::DeserializeOwned>(
    req: Request<Incoming>,
) -> Result<T, Response<BoxBody>> {
    let bytes = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(err) => {
            return Err(fault(
                StatusCode::BAD_REQUEST,
                &format!("failed to read body: {err}"),
            ));
        }
    };
    serde_json::from_slice::<T>(&bytes).map_err(|err| {
        fault(
            StatusCode::BAD_REQUEST,
            &format!("failed to parse JSON body: {err}"),
        )
    })
}

fn to_response(result: Result<(), VmmBackendError>) -> Response<BoxBody> {
    match result {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Full::new(Bytes::new()))
            .unwrap(),
        Err(err) => {
            let status = match err {
                VmmBackendError::InvalidState(_) => StatusCode::BAD_REQUEST,
                VmmBackendError::InvalidConfig(_) => StatusCode::BAD_REQUEST,
                VmmBackendError::NotSupported(_) => StatusCode::BAD_REQUEST,
                VmmBackendError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            fault(status, &err.to_string())
        }
    }
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<BoxBody> {
    let json = serde_json::to_vec(body).expect("backend-owned types must serialize");
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

fn fault(status: StatusCode, msg: &str) -> Response<BoxBody> {
    #[derive(Serialize)]
    struct Fault<'a> {
        fault_message: &'a str,
    }
    json_response(status, &Fault { fault_message: msg })
}

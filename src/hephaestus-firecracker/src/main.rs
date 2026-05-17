//! Firecracker-compatible HTTP API shim for macOS.
//!
//! One process = one microVM, matching upstream's contract. Accepts a
//! UNIX-socket path via `--api-sock` (default
//! `/tmp/hephaestus-firecracker.socket`), listens over hyper/HTTP-1.1, and
//! dispatches into a `VmmBackend` impl (currently the VZ-backed one in
//! `hephaestus-vmm`). Request/response JSON shapes mirror upstream verbatim
//! via the types in `hephaestus-fc-api`.
//!
//! Endpoints wired for v0.3 cold boot:
//!   GET    /
//!   GET    /machine-config
//!   PUT    /machine-config
//!   PATCH  /machine-config
//!   PUT    /boot-source
//!   PUT    /drives/{id}
//!   PUT    /network-interfaces/{id}
//!   PUT    /actions            (InstanceStart only)
//!
//! Everything else returns 400 with a Firecracker-compat error body until
//! a client needs it.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::UnixListener;
use tokio::sync::Mutex;

mod backend;
mod server;

use backend::VzBackend;

#[derive(Parser, Debug)]
#[command(name = "hephaestus-firecracker", version)]
struct Args {
    /// Path to the UNIX socket the HTTP API listens on.
    #[arg(long, default_value = "/tmp/hephaestus-firecracker.socket")]
    api_sock: PathBuf,
    /// MicroVM identifier used in instance-info responses.
    #[arg(long, default_value = "anonymous-instance")]
    id: String,
    /// Optional warm-pool directory built by `hephaestus pool init`.
    /// When set, `InstanceStart` tries to restore from a matching slot
    /// before falling back to cold boot. See `docs/hephaestus-progress.md`
    /// for the agent-init divergence note.
    #[arg(long)]
    pool_dir: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Remove any stale socket from a previous run; UnixListener::bind fails
    // if the path already exists.
    if args.api_sock.exists() {
        std::fs::remove_file(&args.api_sock)?;
    }

    let listener = UnixListener::bind(&args.api_sock)?;
    eprintln!(
        "hephaestus-firecracker listening on {}",
        args.api_sock.display()
    );

    let mut backend = VzBackend::new(args.id);
    if let Some(dir) = args.pool_dir.as_deref() {
        match hephaestus_pool::Pool::open(dir) {
            Ok(pool) => {
                eprintln!(
                    "hephaestus-firecracker: pool attached at {} ({} slots)",
                    pool.dir.display(),
                    pool.meta.slots
                );
                backend = backend.with_pool(pool);
            }
            Err(err) => {
                eprintln!(
                    "hephaestus-firecracker: --pool-dir {} unusable: {err}; running pool-less",
                    dir.display()
                );
            }
        }
    }
    let backend = Arc::new(Mutex::new(backend));
    {
        let backend = backend.clone();
        tokio::task::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                backend.lock().await.flush_metrics();
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let backend = backend.clone();
        tokio::task::spawn(async move {
            if let Err(err) = server::serve_connection(stream, backend).await {
                eprintln!("connection error: {err}");
            }
        });
    }
}

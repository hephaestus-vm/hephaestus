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
mod sandbox;
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
    /// Experimental macOS sandbox profile to enter before serving requests.
    /// The profile must allow the API socket plus all kernel/rootfs/log/snapshot
    /// paths the client will later configure. This is the first jailer hook, not
    /// a full Firecracker jailer replacement yet.
    #[arg(long)]
    sandbox_profile: Option<PathBuf>,
    /// Test-only jailer probe: after entering the sandbox, try to read this
    /// path and fail startup if the read succeeds. Used by restrictive-profile
    /// e2e to prove the sandbox denies paths outside the generated allowlist.
    #[arg(long, hide = true)]
    sandbox_deny_probe: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if let Some(path) = args.sandbox_profile.as_deref() {
        let profile = std::fs::read_to_string(path)?;
        sandbox::apply_profile(&profile)
            .map_err(|err| format!("failed to apply sandbox profile {}: {err}", path.display()))?;
        eprintln!(
            "hephaestus-firecracker: entered sandbox profile {}",
            path.display()
        );
        if let Some(probe) = args.sandbox_deny_probe.as_deref() {
            match std::fs::read(probe) {
                Ok(_) => {
                    return Err(format!(
                        "sandbox deny probe unexpectedly read {}",
                        probe.display()
                    )
                    .into());
                }
                Err(err) => {
                    eprintln!(
                        "hephaestus-firecracker: sandbox deny probe blocked {} ({err})",
                        probe.display()
                    );
                }
            }
        }
    }

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

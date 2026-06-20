//! hephaestus-jailer — per-VM supervisor that generates a deny-by-default
//! macOS sandbox profile and launches `hephaestus-firecracker` under it.
//!
//! What this does:
//! 1. Materializes a per-VM work dir (`--work-dir <dir>/<id>`) holding the
//!    api socket, log file, metrics file, snapshot blob, etc.
//! 2. Canonicalizes the caller-supplied kernel/rootfs/initramfs paths.
//! 3. Generates a deny-by-default sandbox profile granting only those
//!    paths plus the per-VM work dir. The rootfs is granted read/write
//!    because Firecracker root drives are commonly configured writable.
//! 4. Execs `hephaestus-firecracker` with `--sandbox-profile <profile>`
//!    and `--api-sock <work_dir>/api.sock`. The child inherits the jail.
//!
//! What this is NOT (yet):
//! - Not a launchd job. The user runs `hephaestus-jailer` directly; the
//!   child runs as a sibling process under the sandbox. A later iteration
//!   can wrap this in a launchd plist or a longer-lived supervisor that
//!   owns N VMs.
//! - Not a full Firecracker jailer replacement. No uid/gid drop, no
//!   chroot, no cgroup pinning. The sandbox profile is the only isolation
//!   boundary today; macOS sandbox profiles are file/network-scoped, not
//!   process-scoped.
//! - Not entitlement-aware. The child still needs to be ad-hoc signed with
//!   `com.apple.security.virtualization`; the jailer cannot grant that
//!   for you. See `docs/JAILER_MMDS_PLAN.md` for the entitlement roadmap.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use thiserror::Error;

mod profile;

#[derive(Parser, Debug)]
#[command(name = "hephaestus-jailer", version)]
struct Args {
    /// MicroVM identifier used in instance-info responses and the per-VM
    /// work dir name.
    #[arg(long, default_value = "anonymous-instance")]
    id: String,

    /// Parent directory under which `<work-dir>/<id>/` is materialized.
    /// Defaults to `$TMPDIR/hephaestus-jail` (or `/tmp/hephaestus-jail`
    /// when `$TMPDIR` is unset).
    #[arg(long)]
    work_dir: Option<PathBuf>,

    /// Path to the `hephaestus-firecracker` binary to exec. Defaults to
    /// looking up `hephaestus-firecracker` on `$PATH`.
    #[arg(long)]
    firecracker_binary: Option<PathBuf>,

    /// Path to the guest kernel image. Required (the profile grants read
    /// access to this path).
    #[arg(long)]
    kernel: PathBuf,

    /// Path to the guest rootfs ext4 image. Required (the profile grants
    /// read/write access to this path because root drives are commonly
    /// configured writable).
    #[arg(long)]
    rootfs: PathBuf,

    /// Optional path to the initramfs (typically `build/agent.cpio.gz`).
    /// Granted read access if supplied.
    #[arg(long)]
    initramfs: Option<PathBuf>,

    /// Optional warm-pool directory. Granted read/write access under each
    /// slot if supplied, so the daemon can claim/restore from it.
    #[arg(long)]
    pool_dir: Option<PathBuf>,

    /// Test-only probe path: after entering the sandbox, the daemon tries
    /// to read this path and fails startup if the read succeeds. Used by
    /// e2e to prove the sandbox denies paths outside the generated
    /// allowlist.
    #[arg(long)]
    deny_probe: Option<PathBuf>,
}

#[derive(Debug, Error)]
enum JailerError {
    #[error("kernel image not found: {}", path.display())]
    KernelNotFound { path: PathBuf },
    #[error("rootfs not found: {}", path.display())]
    RootfsNotFound { path: PathBuf },
    #[error("firecracker binary not found: {}", path.display())]
    BinaryNotFound { path: PathBuf },
    #[error("failed to generate sandbox profile: {0}")]
    Profile(#[from] profile::GenError),
    #[error("failed to write profile {}: {source}", path.display())]
    WriteProfile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create work dir {}: {source}", path.display())]
    CreateWorkDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to exec {}: {source}", binary.display())]
    Exec {
        binary: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            eprintln!("hephaestus-jailer: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<u8, JailerError> {
    if !args.kernel.exists() {
        return Err(JailerError::KernelNotFound { path: args.kernel });
    }
    if !args.rootfs.exists() {
        return Err(JailerError::RootfsNotFound { path: args.rootfs });
    }

    // Resolve the binary to exec. Default to `hephaestus-firecracker` on
    // $PATH; the user can override with --firecracker-binary for tests or
    // when running from a build dir.
    let binary = args
        .firecracker_binary
        .clone()
        .or_else(|| which("hephaestus-firecracker"))
        .ok_or_else(|| JailerError::BinaryNotFound {
            path: PathBuf::from("hephaestus-firecracker"),
        })?;
    if !binary.exists() {
        return Err(JailerError::BinaryNotFound { path: binary });
    }

    // Materialize the per-VM work dir. The api socket, log, metrics, and
    // snapshot files all live under here; the sandbox profile grants
    // read/write/create/delete on the whole subtree so the daemon can
    // create them without us having to enumerate each one upfront.
    let work_root = args.work_dir.clone().unwrap_or_else(default_work_root);
    let work_dir = work_root.join(&args.id);
    std::fs::create_dir_all(&work_dir).map_err(|source| JailerError::CreateWorkDir {
        path: work_dir.clone(),
        source,
    })?;

    let api_sock = work_dir.join("api.sock");
    let profile_path = work_dir.join("sandbox.profile");

    // Generate the sandbox profile. We grant:
    // - read-only on the kernel and initramfs (caller-supplied inputs)
    // - read/write on the rootfs file (root drives are commonly writable)
    // - read/write on the work_dir subtree (api socket, logs, metrics, snapshots)
    // - read/write on the pool_dir subtree if --pool-dir is set
    let mut reads: Vec<&Path> = vec![&args.kernel];
    if let Some(initramfs) = args.initramfs.as_deref() {
        reads.push(initramfs);
    }
    let read_write_files: Vec<&Path> = vec![&args.rootfs];
    let mut work_dirs: Vec<&Path> = vec![work_dir.as_path()];
    if let Some(pool_dir) = args.pool_dir.as_deref() {
        // Pool dir is also a work dir — the daemon clones rootfs into a
        // slot subdir and reads/writes the snapshot blob.
        work_dirs.push(pool_dir);
    }
    let inputs = profile::ProfileInputs {
        work_dirs,
        read_write_files,
        reads,
    };
    let profile_source = profile::generate(&inputs)?;
    std::fs::write(&profile_path, profile_source).map_err(|source| JailerError::WriteProfile {
        path: profile_path.clone(),
        source,
    })?;
    eprintln!(
        "hephaestus-jailer: wrote profile to {}",
        profile_path.display()
    );

    // Exec the firecracker binary under the generated profile. The child
    // enters the sandbox before serving the API socket, so every API
    // request is bound by the profile.
    let mut cmd = Command::new(&binary);
    cmd.env("HEPHAESTUS_FC_WORK_DIR", &work_dir);
    cmd.arg("--api-sock").arg(&api_sock);
    cmd.arg("--id").arg(&args.id);
    cmd.arg("--sandbox-profile").arg(&profile_path);
    if let Some(pool_dir) = args.pool_dir.as_deref() {
        cmd.arg("--pool-dir").arg(pool_dir);
    }
    if let Some(probe) = args.deny_probe.as_deref() {
        cmd.arg("--sandbox-deny-probe").arg(probe);
    }
    eprintln!(
        "hephaestus-jailer: exec {} {}",
        binary.display(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let status = cmd.status().map_err(|source| JailerError::Exec {
        binary: binary.clone(),
        source,
    })?;
    Ok(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1))
}

/// `which(1)` equivalent — look up a binary on `$PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Default work root: `$TMPDIR/hephaestus-jail` or `/tmp/hephaestus-jail`.
fn default_work_root() -> PathBuf {
    std::env::temp_dir().join("hephaestus-jail")
}

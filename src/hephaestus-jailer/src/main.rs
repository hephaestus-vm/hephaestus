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

/// Everything the jailer resolves before exec: the binary to run, the per-VM
/// work dir, and the paths it materializes inside it. Split out from `run` so
/// the preparation — input validation, work-dir creation, and profile
/// generation — is unit-testable without actually exec'ing the daemon.
struct Plan {
    binary: PathBuf,
    work_dir: PathBuf,
    api_sock: PathBuf,
    profile_path: PathBuf,
}

/// Validate inputs, materialize the per-VM work dir, and write the generated
/// sandbox profile. Returns the resolved paths; performs no exec.
fn prepare(args: &Args) -> Result<Plan, JailerError> {
    if !args.kernel.exists() {
        return Err(JailerError::KernelNotFound {
            path: args.kernel.clone(),
        });
    }
    if !args.rootfs.exists() {
        return Err(JailerError::RootfsNotFound {
            path: args.rootfs.clone(),
        });
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

    Ok(Plan {
        binary,
        work_dir,
        api_sock,
        profile_path,
    })
}

/// Build the `hephaestus-firecracker` command line from a prepared plan. Pure
/// (no side effects, no exec) so a test can assert the args/env it produces.
fn build_command(plan: &Plan, args: &Args) -> Command {
    let mut cmd = Command::new(&plan.binary);
    cmd.env("HEPHAESTUS_FC_WORK_DIR", &plan.work_dir);
    cmd.arg("--api-sock").arg(&plan.api_sock);
    cmd.arg("--id").arg(&args.id);
    cmd.arg("--sandbox-profile").arg(&plan.profile_path);
    if let Some(pool_dir) = args.pool_dir.as_deref() {
        cmd.arg("--pool-dir").arg(pool_dir);
    }
    if let Some(probe) = args.deny_probe.as_deref() {
        cmd.arg("--sandbox-deny-probe").arg(probe);
    }
    cmd
}

fn run(args: Args) -> Result<u8, JailerError> {
    let plan = prepare(&args)?;

    // Exec the firecracker binary under the generated profile. The child
    // enters the sandbox before serving the API socket, so every API
    // request is bound by the profile.
    let mut cmd = build_command(&plan, &args);
    eprintln!(
        "hephaestus-jailer: exec {} {}",
        plan.binary.display(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let status = cmd.status().map_err(|source| JailerError::Exec {
        binary: plan.binary.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Fresh, per-test scratch dir. Unique by test tag + pid so parallel
    /// tests don't collide; wiped on entry so reruns start clean. Avoids a
    /// tempfile dependency (the crate ships only clap + thiserror).
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("heph-jailer-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: PathBuf) -> PathBuf {
        fs::write(&path, b"x").unwrap();
        path
    }

    /// Args with all required inputs existing under `dir` and an explicit
    /// fake firecracker binary, so `prepare` never falls back to `$PATH`.
    fn args_in(dir: &Path) -> Args {
        Args {
            id: "vm-test".into(),
            work_dir: Some(dir.join("work")),
            firecracker_binary: Some(touch(dir.join("fake-firecracker"))),
            kernel: touch(dir.join("vmlinux")),
            rootfs: touch(dir.join("rootfs.ext4")),
            initramfs: None,
            pool_dir: None,
            deny_probe: None,
        }
    }

    fn arg_strings(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn prepare_rejects_missing_kernel() {
        let dir = scratch("missing-kernel");
        let mut args = args_in(&dir);
        args.kernel = dir.join("no-such-kernel");
        assert!(matches!(
            prepare(&args),
            Err(JailerError::KernelNotFound { .. })
        ));
    }

    #[test]
    fn prepare_rejects_missing_rootfs() {
        let dir = scratch("missing-rootfs");
        let mut args = args_in(&dir);
        args.rootfs = dir.join("no-such-rootfs");
        assert!(matches!(
            prepare(&args),
            Err(JailerError::RootfsNotFound { .. })
        ));
    }

    #[test]
    fn prepare_rejects_missing_binary() {
        let dir = scratch("missing-binary");
        let mut args = args_in(&dir);
        args.firecracker_binary = Some(dir.join("no-such-binary"));
        assert!(matches!(
            prepare(&args),
            Err(JailerError::BinaryNotFound { .. })
        ));
    }

    #[test]
    fn prepare_materializes_work_dir_and_profile() {
        let dir = scratch("happy");
        let args = args_in(&dir);
        let plan = prepare(&args).expect("prepare should succeed");

        assert!(plan.work_dir.is_dir(), "work dir should be created");
        assert_eq!(plan.work_dir.file_name().unwrap(), "vm-test");
        assert_eq!(plan.api_sock, plan.work_dir.join("api.sock"));
        assert_eq!(plan.profile_path, plan.work_dir.join("sandbox.profile"));

        let profile = fs::read_to_string(&plan.profile_path).expect("profile written");
        assert!(!profile.is_empty(), "profile should be non-empty");
        // The rootfs is granted read/write, so its name appears in the profile.
        assert!(
            profile.contains("rootfs.ext4"),
            "profile should grant the rootfs path"
        );
    }

    #[test]
    fn build_command_wires_core_args_and_env() {
        let dir = scratch("cmd-core");
        let args = args_in(&dir);
        let plan = prepare(&args).unwrap();
        let cmd = build_command(&plan, &args);

        let got = arg_strings(&cmd);
        assert!(
            got.windows(2)
                .any(|w| w == ["--id".to_string(), "vm-test".to_string()])
        );
        assert!(got.iter().any(|a| a == "--api-sock"));
        assert!(got.iter().any(|a| a == "--sandbox-profile"));
        // pool-dir / deny-probe omitted when their args are None.
        assert!(!got.iter().any(|a| a == "--pool-dir"));
        assert!(!got.iter().any(|a| a == "--sandbox-deny-probe"));

        let work_env = cmd
            .get_envs()
            .find(|(k, _)| *k == "HEPHAESTUS_FC_WORK_DIR")
            .and_then(|(_, v)| v)
            .map(PathBuf::from);
        assert_eq!(work_env.as_deref(), Some(plan.work_dir.as_path()));
    }

    #[test]
    fn build_command_passes_pool_dir_and_deny_probe_when_set() {
        let dir = scratch("cmd-opts");
        let mut args = args_in(&dir);
        // pool_dir is a directory (generate's subpath_form create_dir_all's it);
        // deny_probe is just passed through as a CLI arg, never materialized.
        args.pool_dir = Some(dir.join("pool"));
        args.deny_probe = Some(dir.join("secret"));
        let plan = prepare(&args).unwrap();
        let cmd = build_command(&plan, &args);

        let got = arg_strings(&cmd);
        assert!(got.iter().any(|a| a == "--pool-dir"));
        assert!(got.iter().any(|a| a == "--sandbox-deny-probe"));
    }
}

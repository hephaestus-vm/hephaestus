//! hephaestus-jailer — per-VM supervisor that generates a deny-by-default
//! macOS sandbox profile and launches `hephaestus-firecracker` under it.
//!
//! What this does:
//! 1. Validates `--id` (Firecracker's `[A-Za-z0-9_-]{1,64}`) so it is a safe
//!    single path component, and materializes a private per-VM work dir
//!    (`<work-root>/<id>/`) holding the api socket, log, metrics, snapshot.
//! 2. Generates a deny-by-default sandbox profile granting only the
//!    caller-supplied paths (kernel/initramfs read-only, rootfs read/write,
//!    pool base read-only + pool slots read/write) plus the per-VM work dir.
//!    Paths are canonicalized during profile generation (see `profile.rs`).
//! 3. Execs `hephaestus-firecracker` with `--sandbox-profile <profile>`
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

use std::os::unix::fs::PermissionsExt;
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
    #[error(
        "invalid --id {id:?}: must match [A-Za-z0-9_-]{{1,64}} (the id becomes a \
         work-dir path component and a sandbox grant, so it must be a single safe name)"
    )]
    InvalidId { id: String },
    #[error("refusing unsafe work dir {}: {reason}", path.display())]
    UnsafeWorkDir { path: PathBuf, reason: &'static str },
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
    // The id becomes a path component of the per-VM work dir and is emitted
    // verbatim into the sandbox profile. An unvalidated id like `../../etc`
    // would path-traverse out of the work root and widen the deny-by-default
    // grant to an arbitrary directory, so we require Firecracker's charset
    // (which also guarantees a single, safe path component).
    validate_id(&args.id)?;

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
    secure_work_root(&work_root)?;
    let work_dir = work_root.join(&args.id);
    // Refuse a pre-planted symlink at the exact work-dir path: `create_dir_all`
    // follows symlinks, so without this a local user could seed
    // `<root>/<id>` → victim dir and have the profile grant RW there. The id
    // is validated above, so this only guards the leaf name.
    if let Ok(meta) = std::fs::symlink_metadata(&work_dir)
        && meta.file_type().is_symlink()
    {
        return Err(JailerError::UnsafeWorkDir {
            path: work_dir,
            reason: "path is a symlink",
        });
    }
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
    // Least privilege for the warm pool: the daemon only *reads* the
    // immutable pool base (save.bin, pristine.ext4, save.machineid, meta) and
    // *writes* inside the per-slot dirs it clones a rootfs into and flocks.
    // Granting RW over the whole subtree would let a compromised daemon
    // overwrite the snapshot every other tenant restores from. Slots are
    // pre-created by `just pool-init`; the daemon never creates new ones.
    let mut pool_read_dirs: Vec<PathBuf> = Vec::new();
    let mut pool_slot_dirs_rw: Vec<PathBuf> = Vec::new();
    if let Some(pool_dir) = args.pool_dir.as_deref() {
        pool_read_dirs.push(pool_dir.to_path_buf());
        pool_slot_dirs_rw = pool_slot_dirs(pool_dir);
        if pool_slot_dirs_rw.is_empty() {
            eprintln!(
                "hephaestus-jailer: warning: --pool-dir {} has no slot-* dirs; \
                 run `just pool-init` first or the pool will always miss",
                pool_dir.display()
            );
        }
    }

    let mut reads: Vec<&Path> = vec![&args.kernel];
    if let Some(initramfs) = args.initramfs.as_deref() {
        reads.push(initramfs);
    }
    let read_write_files: Vec<&Path> = vec![&args.rootfs];
    let mut work_dirs: Vec<&Path> = vec![work_dir.as_path()];
    for slot in &pool_slot_dirs_rw {
        work_dirs.push(slot.as_path());
    }
    let read_dirs: Vec<&Path> = pool_read_dirs.iter().map(PathBuf::as_path).collect();
    let inputs = profile::ProfileInputs {
        work_dirs,
        read_write_files,
        reads,
        read_dirs,
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

/// Enforce Firecracker's instance-id charset (`[A-Za-z0-9_-]{1,64}`). This
/// doubles as a "single safe path component" check: the charset excludes `/`,
/// `.`, and `..`, so a validated id can never traverse out of the work root.
fn validate_id(id: &str) -> Result<(), JailerError> {
    let ok = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(JailerError::InvalidId { id: id.to_string() })
    }
}

/// Ensure the shared work root is a private, non-symlink directory owned by
/// us. The default root lives under world-writable `/tmp`, so a local attacker
/// could otherwise pre-plant `hephaestus-jail` as a symlink to a victim dir
/// (which `create_dir_all` would follow, widening the sandbox grant) or seed a
/// guessable `<root>/<id>` for us to descend into. Forcing `0700` fails closed
/// (EPERM) if another user already owns the path.
fn secure_work_root(root: &Path) -> Result<(), JailerError> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(JailerError::UnsafeWorkDir {
                    path: root.to_path_buf(),
                    reason: "work root is a symlink",
                });
            }
            if !meta.is_dir() {
                return Err(JailerError::UnsafeWorkDir {
                    path: root.to_path_buf(),
                    reason: "work root exists but is not a directory",
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root).map_err(|source| JailerError::CreateWorkDir {
                path: root.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(JailerError::CreateWorkDir {
                path: root.to_path_buf(),
                source,
            });
        }
    }
    // Enforce private perms on every run. If we don't own the directory this
    // fails with EPERM, which is the fail-closed outcome we want.
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|_| {
        JailerError::UnsafeWorkDir {
            path: root.to_path_buf(),
            reason: "cannot enforce private 0700 perms (not owner?)",
        }
    })
}

/// Existing `slot-*` subdirectories of a warm pool. Pool slots are pre-created
/// by `just pool-init`; the daemon only clones a rootfs into and flocks an
/// existing slot, so enumerating them lets us grant each read/write while
/// keeping the pool base read-only. Returns empty on an unreadable/absent pool.
fn pool_slot_dirs(pool_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(pool_dir) else {
        return Vec::new();
    };
    let mut slots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("slot-"))
        })
        .collect();
    slots.sort();
    slots
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
        // Pool base is granted read-only now, so it (and at least one slot)
        // must pre-exist — the daemon never creates them. deny_probe is just
        // passed through as a CLI arg, never materialized.
        let pool = dir.join("pool");
        fs::create_dir_all(pool.join("slot-0")).unwrap();
        args.pool_dir = Some(pool);
        args.deny_probe = Some(dir.join("secret"));
        let plan = prepare(&args).unwrap();
        let cmd = build_command(&plan, &args);

        let got = arg_strings(&cmd);
        assert!(got.iter().any(|a| a == "--pool-dir"));
        assert!(got.iter().any(|a| a == "--sandbox-deny-probe"));
    }

    #[test]
    fn validate_id_accepts_firecracker_charset_and_rejects_traversal() {
        for ok in ["vm-test", "a", "ci_runner_42", &"x".repeat(64)] {
            assert!(validate_id(ok).is_ok(), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            &"x".repeat(65),
            "../etc",
            "a/b",
            "..",
            ".",
            "has space",
            "tab\t",
            "dot.dot",
        ] {
            assert!(
                matches!(validate_id(bad), Err(JailerError::InvalidId { .. })),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn prepare_rejects_traversal_id_before_touching_the_filesystem() {
        let dir = scratch("traversal-id");
        let mut args = args_in(&dir);
        args.id = "../../escape".into();
        assert!(matches!(prepare(&args), Err(JailerError::InvalidId { .. })));
    }

    #[test]
    fn prepare_grants_pool_base_read_only_and_slots_read_write() {
        let dir = scratch("pool-split");
        let mut args = args_in(&dir);
        let pool = dir.join("pool");
        // Immutable base file + two pre-created slots.
        fs::create_dir_all(&pool).unwrap();
        touch(pool.join("save.bin"));
        fs::create_dir_all(pool.join("slot-0")).unwrap();
        fs::create_dir_all(pool.join("slot-1")).unwrap();
        args.pool_dir = Some(pool.clone());

        let plan = prepare(&args).unwrap();
        let profile = fs::read_to_string(&plan.profile_path).unwrap();
        let pool_canon = fs::canonicalize(&pool).unwrap();

        // Pool base appears under the read-only grant, NOT the read/write one.
        assert!(
            profile.contains(";; Read-only directory subtrees"),
            "expected a read-only subtree section:\n{profile}"
        );
        let rw_section = profile
            .split(";; Per-VM working directories/files")
            .nth(1)
            .unwrap_or("");
        assert!(
            !rw_section.contains(&format!("\"{}\"\n", pool_canon.to_string_lossy())),
            "pool base must not be in the read/write grant:\n{profile}"
        );
        // Both slots are granted read/write.
        for slot in ["slot-0", "slot-1"] {
            let slot_canon = fs::canonicalize(pool.join(slot)).unwrap();
            assert!(
                rw_section.contains(&slot_canon.to_string_lossy().to_string()),
                "{slot} should be read/write:\n{profile}"
            );
        }
    }
}

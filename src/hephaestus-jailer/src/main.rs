//! hephaestus-jailer — per-VM supervisor that generates a deny-by-default
//! macOS sandbox profile and launches `hephaestus-firecracker` under it.
//!
//! What this does:
//! 1. Validates `--id` (Firecracker's `[A-Za-z0-9_-]{1,64}`) so it is a safe
//!    single path component, and materializes a private per-VM work dir
//!    (`<work-root>/<id>/`) holding the api socket, log, metrics, snapshot.
//! 2. Claims the instance: an exclusive `flock` on `<work-root>/.<id>.lock`
//!    refuses a second jailer for the same id (which would otherwise steal
//!    the live api socket). The lock fd is inherited by the daemon, so the
//!    claim survives even a SIGKILLed jailer for as long as the daemon runs.
//! 3. Removes a stale `api.sock` left by a previous run. Everything else in
//!    the work dir (logs, metrics, snapshots) persists across restarts;
//!    `--clean-work-dir` empties it first, and `--teardown` retires the
//!    instance's on-disk state entirely instead of launching.
//! 4. Generates a deny-by-default sandbox profile granting only the
//!    caller-supplied paths (kernel/initramfs read-only, rootfs read/write,
//!    pool base read-only + pool slots read/write) plus the per-VM work dir.
//!    Paths are canonicalized during profile generation (see `profile.rs`).
//! 5. Optionally drops privileges to an unprivileged uid/gid (`--uid`,
//!    `--gid`, `--user`), or allocates a dedicated per-instance uid from
//!    `--uid-base` (recorded in the root-owned `.uid-allocations` registry;
//!    stable across restarts, released by `--teardown`). Requires root. A
//!    launch is refused while another live instance runs under the same
//!    uid (`--allow-shared-uid` overrides). The per-VM work dir is chown'd
//!    to the target so the daemon can create the api socket after the drop;
//!    the sandbox profile stays root-owned outside that writable directory.
//! 6. Execs `hephaestus-firecracker` with `--sandbox-profile <profile>`
//!    and `--api-sock <work_dir>/api.sock`. The child inherits the jail.
//!
//! The daemon is launched as its own process-group leader, and the jailer
//! forwards `SIGTERM`/`SIGINT` to that group — so terminating the jailer
//! reaps the daemon (and its VM) rather than orphaning it to launchd. A hard
//! `SIGKILL` to the jailer can still orphan the daemon (macOS has no
//! `PR_SET_PDEATHSIG`); a launchd-owned supervisor is the eventual fix.
//!
//! With `--generate-launchd-plist`, the jailer writes a launchd plist to
//! stdout (or `--launchd-plist-path`) instead of running. The plist wraps
//! the same jailer invocation with `KeepAlive`, restarting the full jailer
//! whenever the daemon exits.
//!
//! Privilege drop: `--uid`, `--gid`, and `--user` drop root privileges
//! before exec'ing the daemon. Requires the jailer to be started as root.
//! The order is setgroups → setgid → setuid: supplementary groups must be
//! cleared while still root, and gid before uid (once uid is dropped,
//! setgid would fail).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use clap::Parser;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use thiserror::Error;

mod profile;
mod uid_registry;

/// Width of the `--uid-base` allocation window. 1000 instances per work
/// root is far beyond what one host runs; a fixed width keeps the flag
/// surface minimal.
const UID_ALLOC_RANGE: libc::uid_t = 1000;

/// Process-group id of the launched `hephaestus-firecracker` child (equal to
/// its pid, since we make it a group leader). Read from a signal handler, so
/// it lives in an atomic. `0` means "no child yet".
static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

/// Forward a termination signal to the child's whole process group so the
/// daemon (and the VM it owns) dies with the jailer instead of reparenting to
/// launchd and leaking. Async-signal-safe: an atomic load + `kill(2)`.
extern "C" fn forward_termination(_sig: libc::c_int) {
    let pgid = CHILD_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        // SAFETY: kill(2) is async-signal-safe; negative pid targets the group.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
}

/// Install `SIGTERM`/`SIGINT` forwarders. Best-effort — `SIGKILL` can't be
/// caught, so a hard-killed jailer can still orphan its daemon (no macOS
/// `PR_SET_PDEATHSIG` equivalent); the common graceful-kill path is covered.
fn install_signal_forwarding() {
    let handler = forward_termination as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: registering a signal handler; the handler is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

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

    /// Path to the guest kernel image. Required except with `--teardown`
    /// (the profile grants read access to this path).
    #[arg(long, required_unless_present = "teardown")]
    kernel: Option<PathBuf>,

    /// Path to the guest rootfs ext4 image. Required except with
    /// `--teardown` (the profile grants read/write access to this path
    /// because root drives are commonly configured writable).
    #[arg(long, required_unless_present = "teardown")]
    rootfs: Option<PathBuf>,

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

    /// Cap the daemon's open file descriptors (`RLIMIT_NOFILE`). Opt-in
    /// hardening; unset leaves the inherited limit. Only lowers, so it needs
    /// no privilege.
    #[arg(long)]
    rlimit_nofile: Option<u64>,

    /// Cap the daemon's process/thread count (`RLIMIT_NPROC`). Opt-in.
    #[arg(long)]
    rlimit_nproc: Option<u64>,

    /// Cap the size (bytes) of any file the daemon can create (`RLIMIT_FSIZE`).
    /// Opt-in.
    #[arg(long)]
    rlimit_fsize: Option<u64>,

    /// Numeric UID to drop privileges to before exec. Requires root (or
    /// the appropriate privilege) — the jailer must be started as root
    /// or with `sudo` for this to succeed. Mutually exclusive with
    /// `--user`.
    #[arg(long, conflicts_with = "user")]
    uid: Option<libc::uid_t>,

    /// Numeric GID to drop privileges to before exec. Requires root.
    /// May be used alone or with `--uid`. Mutually exclusive with
    /// `--user`.
    #[arg(long, conflicts_with = "user")]
    gid: Option<libc::gid_t>,

    /// Username to look up and drop privileges to (sets both uid and
    /// gid from the user's passwd entry). Requires root. Mutually
    /// exclusive with `--uid` and `--gid`.
    #[arg(long, conflicts_with_all = ["uid", "gid"])]
    user: Option<String>,

    /// Base of a per-instance dedicated-uid range: each instance id is
    /// allocated — and keeps, across restarts — its own uid from
    /// [base, base+1000), with gid set equal to the uid. The recommended
    /// mode for running multiple VMs: instances sharing one uid can signal
    /// and ptrace each other. Requires root. Mutually exclusive with
    /// `--uid`/`--gid`/`--user`.
    #[arg(long, conflicts_with_all = ["uid", "gid", "user"])]
    uid_base: Option<libc::uid_t>,

    /// Proceed even when another live instance already runs under the same
    /// uid (the jailer otherwise refuses to launch). Escape hatch for
    /// deliberate shared-uid deployments; weakens instance separation.
    #[arg(long)]
    allow_shared_uid: bool,

    /// Instead of running, generate a launchd plist that wraps this
    /// jailer invocation and write it to stdout. The plist uses
    /// `KeepAlive` so launchd restarts the full jailer whenever the
    /// daemon exits.
    #[arg(long)]
    generate_launchd_plist: bool,

    /// Path to write the launchd plist to (implies
    /// `--generate-launchd-plist`). Defaults to stdout when
    /// `--generate-launchd-plist` is used without this flag.
    #[arg(long)]
    launchd_plist_path: Option<PathBuf>,

    /// Empty the per-VM work dir before launch, discarding stale sockets,
    /// logs, metrics, and snapshots. One-shot operator action; deliberately
    /// never emitted into generated launchd plists, so `KeepAlive` restarts
    /// preserve state.
    #[arg(long)]
    clean_work_dir: bool,

    /// Instead of launching, remove this instance's on-disk state — the
    /// per-VM work dir, sandbox profile, instance lock, launchd logs, and
    /// its uid-registry entry — and exit. Refuses while the instance is
    /// running (its lock is held). Unload any launchd plist first
    /// (`launchctl bootout`), or launchd will simply re-create everything
    /// on the next restart.
    #[arg(
        long,
        conflicts_with_all = ["generate_launchd_plist", "launchd_plist_path", "clean_work_dir"]
    )]
    teardown: bool,
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
    #[error("failed to write launchd plist to {}: {source}", path.display())]
    WritePlist {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to resolve privilege-drop target: {source}")]
    PrivilegeDrop {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to chown {} for privilege drop: {source}", path.display())]
    Chown {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to resolve the jailer executable path: {source}")]
    CurrentExe {
        #[source]
        source: std::io::Error,
    },
    #[error(
        "instance {id:?} is already running (lock held on {}): stop it first \
         (kill its jailer, or `launchctl bootout` its plist)",
        lock_path.display()
    )]
    InstanceBusy { id: String, lock_path: PathBuf },
    #[error("failed to acquire instance lock {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove {}: {source}", path.display())]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("uid registry: {0}")]
    UidRegistry(#[from] uid_registry::RegistryError),
    #[error(
        "refusing to run instance {id:?} as uid {uid}: instance {other_id:?} is live \
         under the same uid (same-uid daemons can signal and ptrace each other; pass \
         --allow-shared-uid to override)"
    )]
    SharedUidLive {
        id: String,
        other_id: String,
        uid: libc::uid_t,
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
    work_root: PathBuf,
    work_dir: PathBuf,
    api_sock: PathBuf,
    profile_path: PathBuf,
    /// The uid/gid the daemon will run as: allocated from `--uid-base`, or
    /// the resolved `--uid`/`--gid`/`--user` values, or `None` (no drop).
    target_uid: Option<libc::uid_t>,
    target_gid: Option<libc::gid_t>,
    /// Exclusive claim on this instance id. Unlocks on drop, and the child
    /// inherits the same open file description, so `run` must keep the plan
    /// alive past `child.wait()` — dropping it early would release the
    /// daemon's claim too.
    lock: Flock<File>,
}

/// Whether a real user account owns `uid`. Allocation skips such uids so a
/// dedicated-uid VM never aliases an existing account. Runs in the parent,
/// pre-fork — like `getpwnam`, `getpwuid` may allocate and must never run
/// in the child. Note this check fails open (a lookup error looks like a
/// free uid); it narrows collisions with real accounts but the hard floor
/// is the registry's unconditional refusal of uid 0.
fn passwd_entry_exists(uid: libc::uid_t) -> bool {
    // SAFETY: getpwuid only reads process-global passwd state; the result
    // is only null-checked, never dereferenced.
    unsafe { !libc::getpwuid(uid).is_null() }
}

/// Non-destructively probe whether an instance's flock is currently held.
/// Opens WITHOUT create — an absent lock file means no live holder — and
/// releases the probe lock immediately on drop. Same idiom as the pool's
/// `stats()`.
fn instance_lock_held(lock_path: &Path) -> Result<bool, JailerError> {
    let file = match File::options().read(true).write(true).open(lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(JailerError::Lock {
                path: lock_path.to_path_buf(),
                source,
            });
        }
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_freed) => Ok(false),
        Err((_, Errno::EWOULDBLOCK)) => Ok(true),
        Err((_, errno)) => Err(JailerError::Lock {
            path: lock_path.to_path_buf(),
            source: std::io::Error::from_raw_os_error(errno as i32),
        }),
    }
}

/// Decide the uid/gid this instance will run as, registering it so other
/// launches can see it. MUST be called with the instance lock held. Lock
/// order is fixed and one-way: instance flock first, registry flock second
/// (the registry's liveness probes are non-blocking, so no cycle can
/// block). Registering before probing, inside one registry critical
/// section, is the race invariant: of two concurrent launches picking the
/// same uid, whichever enters the registry second sees the first's entry
/// and its held instance lock, and refuses. The registry flock is released
/// when this function returns — only the instance lock lives as long as
/// the daemon.
fn resolve_instance_identity(
    args: &Args,
    explicit: (Option<libc::uid_t>, Option<libc::gid_t>),
    work_root: &Path,
) -> Result<(Option<libc::uid_t>, Option<libc::gid_t>), JailerError> {
    if let Some(base) = args.uid_base {
        let mut reg = uid_registry::Registry::open(work_root)?;
        let uid = reg.allocate(&args.id, base, UID_ALLOC_RANGE, passwd_entry_exists)?;
        reg.upsert(&args.id, uid)?;
        refuse_live_shared_uid(work_root, &reg, &args.id, uid, args.allow_shared_uid)?;
        // A dedicated uid gets a matching dedicated gid: sharing a group
        // would put every allocated-uid VM back into one mutual-access set.
        return Ok((Some(uid), Some(uid)));
    }
    if let Some(uid) = explicit.0 {
        // Explicit --uid/--user launches register too, so allocation and
        // the liveness refusal see every dropping instance's uid.
        let mut reg = uid_registry::Registry::open(work_root)?;
        reg.upsert(&args.id, uid)?;
        refuse_live_shared_uid(work_root, &reg, &args.id, uid, args.allow_shared_uid)?;
    }
    Ok(explicit)
}

/// Refuse to launch `id` as `uid` while another instance is live under the
/// same uid. The registry names every dropping instance's uid; each other
/// entry with our uid is probed for a held instance lock. The self-skip is
/// load-bearing: flock is per open-file-description, so probing our own
/// (already held) lock from a fresh fd would report *us* as the conflict.
fn refuse_live_shared_uid(
    work_root: &Path,
    registry: &uid_registry::Registry,
    id: &str,
    uid: libc::uid_t,
    allow: bool,
) -> Result<(), JailerError> {
    for (other_id, other_uid) in registry.entries() {
        if other_id == id || *other_uid != uid {
            continue;
        }
        // Entries were charset-validated at parse time, so joining them
        // into a lock-file name cannot traverse; validate again anyway to
        // keep this path safe against future registry format changes.
        if validate_id(other_id).is_err() {
            continue;
        }
        let lock_path = work_root.join(format!(".{other_id}.lock"));
        if instance_lock_held(&lock_path)? {
            if allow {
                eprintln!(
                    "hephaestus-jailer: warning: sharing uid {uid} with live instance \
                     {other_id:?} (--allow-shared-uid)"
                );
                continue;
            }
            return Err(JailerError::SharedUidLive {
                id: id.to_string(),
                other_id: other_id.clone(),
                uid,
            });
        }
    }
    Ok(())
}

/// Claim an instance id by taking an exclusive non-blocking `flock` on its
/// lock file. A held lock means a jailer (or its inherited-fd daemon) is
/// alive for this id, so a second claim refuses instead of stealing the
/// api socket. The file itself persists between runs — unlinking a lock
/// file while others may open it is racy, so only `--teardown` removes it,
/// unlinking before it releases; the inode re-check below closes the
/// remaining window where this open races that unlink.
fn acquire_instance_lock(lock_path: &Path, id: &str) -> Result<Flock<File>, JailerError> {
    let io_err = |source| JailerError::Lock {
        path: lock_path.to_path_buf(),
        source,
    };
    for _ in 0..3 {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(io_err)?;
        let lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => lock,
            Err((_, Errno::EWOULDBLOCK)) => {
                return Err(JailerError::InstanceBusy {
                    id: id.to_string(),
                    lock_path: lock_path.to_path_buf(),
                });
            }
            Err((_, errno)) => {
                return Err(io_err(std::io::Error::from_raw_os_error(errno as i32)));
            }
        };
        // The daemon never opens the lock by path, so it needs no read bit.
        std::fs::set_permissions(lock_path, std::fs::Permissions::from_mode(0o600))
            .map_err(io_err)?;
        // A concurrent teardown may have unlinked the file between our open
        // and the flock, leaving us locking an orphaned inode no future open
        // can see. Verify the path still names the inode we locked; retry
        // with a fresh open otherwise.
        let held = lock.metadata().map_err(io_err)?;
        match std::fs::symlink_metadata(lock_path) {
            Ok(disk) if disk.ino() == held.ino() && disk.dev() == held.dev() => return Ok(lock),
            _ => continue,
        }
    }
    Err(io_err(std::io::Error::other(
        "lock file kept changing underneath (concurrent teardown?)",
    )))
}

/// Validate inputs, materialize the per-VM work dir, and write the generated
/// sandbox profile. Returns the resolved paths; performs no exec.
/// `explicit` is the pre-resolved `--uid`/`--gid`/`--user` target (uid,
/// gid), resolved by the caller so `getpwnam` stays out of this function;
/// `--uid-base` allocation happens here because it must run under the
/// instance lock.
fn prepare(
    args: &Args,
    explicit: (Option<libc::uid_t>, Option<libc::gid_t>),
) -> Result<Plan, JailerError> {
    // The id becomes a path component of the per-VM work dir and is emitted
    // verbatim into the sandbox profile. An unvalidated id like `../../etc`
    // would path-traverse out of the work root and widen the deny-by-default
    // grant to an arbitrary directory, so we require Firecracker's charset
    // (which also guarantees a single, safe path component).
    validate_id(&args.id)?;

    // clap enforces both flags whenever `--teardown` is absent, and teardown
    // is dispatched before `prepare` is ever called.
    let kernel = args.kernel.as_deref().expect("--kernel enforced by clap");
    let rootfs = args.rootfs.as_deref().expect("--rootfs enforced by clap");
    if !kernel.exists() {
        return Err(JailerError::KernelNotFound {
            path: kernel.to_path_buf(),
        });
    }
    if !rootfs.exists() {
        return Err(JailerError::RootfsNotFound {
            path: rootfs.to_path_buf(),
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
    let drop_requested = explicit.0.is_some() || explicit.1.is_some() || args.uid_base.is_some();
    secure_work_root(&work_root, drop_requested)?;

    // Claim the instance before touching any of its state. Like the profile,
    // the lock file is a root-owned dot-sibling outside the (chowned, daemon-
    // writable) work dir, so a compromised dropped-uid daemon cannot unlink
    // or replace it.
    let lock_path = work_root.join(format!(".{}.lock", args.id));
    let lock = acquire_instance_lock(&lock_path, &args.id)?;

    let (target_uid, target_gid) = resolve_instance_identity(args, explicit, &work_root)?;

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
    // One-shot retire-and-recreate: under the lock, so a running instance's
    // state can never be wiped. `remove_dir_all` does not follow the leaf
    // symlink (refused above) or interior symlinks.
    if args.clean_work_dir && std::fs::symlink_metadata(&work_dir).is_ok() {
        std::fs::remove_dir_all(&work_dir).map_err(|source| JailerError::Remove {
            path: work_dir.clone(),
            source,
        })?;
        eprintln!("hephaestus-jailer: cleaned work dir {}", work_dir.display());
    }
    std::fs::create_dir_all(&work_dir).map_err(|source| JailerError::CreateWorkDir {
        path: work_dir.clone(),
        source,
    })?;

    // The daemon owns this directory after a privilege drop. Normalize its
    // mode on every run so a stale invocation cannot make the next launch
    // inaccessible. The parent is root-owned and not writable by the daemon,
    // so the daemon cannot replace this directory with a symlink.
    std::fs::set_permissions(&work_dir, std::fs::Permissions::from_mode(0o700)).map_err(|_| {
        JailerError::UnsafeWorkDir {
            path: work_dir.clone(),
            reason: "cannot enforce private per-VM work-dir permissions",
        }
    })?;

    let api_sock = work_dir.join("api.sock");
    // Remove a stale api socket from a previous run while still privileged
    // and under the lock. `remove_file` unlinks without following, so a
    // daemon-planted symlink at this name is removed, not traversed. The
    // daemon would unlink the path itself before binding, but staleness is
    // this supervisor's job — the daemon's unlink is bare-daemon compat.
    if std::fs::symlink_metadata(&api_sock).is_ok() {
        std::fs::remove_file(&api_sock).map_err(|source| JailerError::Remove {
            path: api_sock.clone(),
            source,
        })?;
        eprintln!(
            "hephaestus-jailer: removed stale api socket {}",
            api_sock.display()
        );
    }
    // Keep the profile out of the daemon-writable work directory. Otherwise a
    // compromised dropped-uid daemon could replace it with a symlink and make
    // the next root launch overwrite or chown an arbitrary path.
    let profile_path = work_root.join(format!(".{}.sandbox.profile", args.id));

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

    let mut reads: Vec<&Path> = vec![kernel];
    if let Some(initramfs) = args.initramfs.as_deref() {
        reads.push(initramfs);
    }
    let read_write_files: Vec<&Path> = vec![rootfs];
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
    // A dropped uid must be able to read the profile before applying it, but
    // only root/the invoking owner may replace it (the parent is 0711/0700).
    let profile_mode = if drop_requested { 0o644 } else { 0o600 };
    std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(profile_mode))
        .map_err(|source| JailerError::WriteProfile {
            path: profile_path.clone(),
            source,
        })?;
    eprintln!(
        "hephaestus-jailer: wrote profile to {}",
        profile_path.display()
    );

    Ok(Plan {
        binary,
        work_root,
        work_dir,
        api_sock,
        profile_path,
        target_uid,
        target_gid,
        lock,
    })
}

/// Build the `hephaestus-firecracker` command line from a prepared plan. No
/// side effects at call time (a test can assert the args/env it produces); it
/// does register a `pre_exec` hook that runs in the child at spawn to put it in
/// its own process group.
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
    // Capture the (Copy) resource caps and drop target so the child closure
    // is self-contained.
    let nofile = args.rlimit_nofile;
    let nproc = args.rlimit_nproc;
    let fsize = args.rlimit_fsize;
    let (target_uid, target_gid) = (plan.target_uid, plan.target_gid);
    // The instance-lock fd is deliberately inherited by the daemon: macOS has
    // no `PR_SET_PDEATHSIG`, so a SIGKILLed jailer orphans the daemon — the
    // shared open file description keeps the flock claim alive for as long as
    // either process lives, and a relaunch cannot steal a live api socket.
    let lock_fd = plan.lock.as_raw_fd();
    // Run the daemon as its own process-group leader (so the jailer can signal
    // the whole subtree on teardown), apply any resource caps, and drop
    // privileges before exec. The uid/gid were resolved in the parent (a
    // `getpwnam` lookup allocates and must never run post-fork).
    // SAFETY: `setpgid`/`fcntl`/`setrlimit`/`setgroups`/`setgid`/`setuid` are
    // async-signal-safe and touch no Rust heap state; the closure only reads
    // captured Copy values.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Clear FD_CLOEXEC so the lock survives the exec (see above).
            if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            apply_rlimit(libc::RLIMIT_NOFILE, nofile)?;
            apply_rlimit(libc::RLIMIT_NPROC, nproc)?;
            apply_rlimit(libc::RLIMIT_FSIZE, fsize)?;
            drop_privileges(target_uid, target_gid)?;
            Ok(())
        });
    }
    cmd
}

/// Lower a resource limit (both soft and hard) to `value`, if set. Lowering is
/// always permitted, so no privilege is required; raising above the current
/// hard limit would need root and is not attempted here. Called from the
/// child's `pre_exec`, so it must stay async-signal-safe (`setrlimit` is).
fn apply_rlimit(resource: libc::c_int, value: Option<u64>) -> std::io::Result<()> {
    let Some(v) = value else {
        return Ok(());
    };
    let rl = libc::rlimit {
        rlim_cur: v as libc::rlim_t,
        rlim_max: v as libc::rlim_t,
    };
    // SAFETY: `rl` is fully initialized; setrlimit is async-signal-safe.
    if unsafe { libc::setrlimit(resource, &rl) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Drop privileges from root to an unprivileged uid/gid before exec. Called
/// from the child's `pre_exec`, so it must stay async-signal-safe: no
/// allocation, only raw syscalls on `Copy` values resolved before fork.
/// The order matters: setgroups → setgid → setuid. Supplementary groups
/// must be cleared while we still have privilege, and gid before uid (once
/// uid is dropped, setgid would fail).
fn drop_privileges(uid: Option<libc::uid_t>, gid: Option<libc::gid_t>) -> std::io::Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    // Replace root's supplementary groups with exactly the target gid (or
    // the current gid when only `--uid` was given). Without this the
    // "unprivileged" daemon keeps wheel/admin/... membership inherited from
    // root, so group-based access is never actually dropped.
    // SAFETY: getgid is an async-signal-safe raw syscall.
    let groups_gid = gid.unwrap_or_else(|| unsafe { libc::getgid() });
    // SAFETY: setgroups is async-signal-safe; the pointer references one gid.
    if unsafe { libc::setgroups(1, &groups_gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if let Some(g) = gid {
        // SAFETY: setgid is async-signal-safe.
        if unsafe { libc::setgid(g) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(u) = uid {
        // SAFETY: setuid is async-signal-safe.
        if unsafe { libc::setuid(u) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // setuid from root drops real, effective, and saved uid together, so
        // regaining root must now fail. `from_raw_os_error` because we are
        // post-fork and must not allocate an error message.
        // SAFETY: setuid is async-signal-safe; a success here is the failure.
        if u != 0 && unsafe { libc::setuid(0) } == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
    }
    Ok(())
}

/// Resolve the target uid/gid from the three possible privilege-drop inputs.
/// When `--user` is given, looks up the passwd entry. Otherwise uses the
/// explicit `--uid`/`--gid` values (which may be partial). `getpwnam` may
/// allocate, so this runs in the parent (from `run`, before spawn); only the
/// resolved `Copy` values cross into the child's `pre_exec`.
fn resolve_user(
    user: Option<&str>,
    uid: Option<libc::uid_t>,
    gid: Option<libc::gid_t>,
) -> std::io::Result<(Option<libc::uid_t>, Option<libc::gid_t>)> {
    if let Some(name) = user {
        let cname = std::ffi::CString::new(name).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "username contains NUL")
        })?;
        // SAFETY: cname is a valid NUL-terminated string for the lookup.
        let entry = unsafe { libc::getpwnam(cname.as_ptr()) };
        if entry.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("user {name:?} not found"),
            ));
        }
        // SAFETY: entry is non-null, so the fields are valid.
        let pw = unsafe { *entry };
        Ok((Some(pw.pw_uid), Some(pw.pw_gid)))
    } else {
        Ok((uid, gid))
    }
}

/// Minimal XML text-node escaping for plist string values. Paths and
/// arguments are interpolated into the plist verbatim otherwise, and a
/// stray `&` or `<` would corrupt the document.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a path for embedding in the plist. launchd runs jobs from `/`, not
/// the shell the plist was generated in, so a relative path would break at
/// load time — canonicalize when the path resolves, and fall back to it as
/// given (e.g. a not-yet-created target) otherwise.
fn plist_path(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Generate a launchd plist that wraps this jailer invocation. The plist
/// uses `KeepAlive` so launchd restarts the full jailer whenever the daemon
/// exits. Intended for
/// `/Library/LaunchDaemons`: the jailer must start as root to generate the
/// sandbox profile and drop privileges; the daemon itself then runs as
/// `--user`/`--uid`/`--gid`.
fn generate_launchd_plist(args: &Args, plan: &Plan) -> Result<String, JailerError> {
    let label = format!("com.hephaestus.vm.{}", args.id);
    // Reconstruct the jailer command line for launchd. argv[0] must be this
    // jailer executable, not the daemon — launchd re-runs the whole jail
    // setup on each restart. The daemon path `prepare` resolved is pinned
    // via --firecracker-binary so a $PATH lookup at load time cannot pick a
    // different binary.
    let jailer_exe =
        std::env::current_exe().map_err(|source| JailerError::CurrentExe { source })?;
    // `--clean-work-dir` is deliberately never reconstructed here: a
    // `KeepAlive` plist carrying it would wipe snapshots on every restart.
    let kernel = args.kernel.as_deref().expect("--kernel enforced by clap");
    let rootfs = args.rootfs.as_deref().expect("--rootfs enforced by clap");
    let mut program_args = vec![
        jailer_exe.to_string_lossy().into_owned(),
        format!("--id={}", args.id),
        format!("--kernel={}", plist_path(kernel)),
        format!("--rootfs={}", plist_path(rootfs)),
        format!("--work-dir={}", plist_path(&plan.work_root)),
        format!("--firecracker-binary={}", plist_path(&plan.binary)),
    ];
    if let Some(initramfs) = args.initramfs.as_deref() {
        program_args.push(format!("--initramfs={}", plist_path(initramfs)));
    }
    if let Some(pool_dir) = args.pool_dir.as_deref() {
        program_args.push(format!("--pool-dir={}", plist_path(pool_dir)));
    }
    if let Some(probe) = args.deny_probe.as_deref() {
        program_args.push(format!("--deny-probe={}", plist_path(probe)));
    }
    if args.uid_base.is_some() {
        // Pin the allocated identity (like --firecracker-binary): restarts
        // must land on the same uid without re-running allocation, and
        // --uid-base itself is deliberately never reconstructed. Explicit
        // launches re-register on every run, so the registry entry
        // self-heals even if the file is lost.
        let uid = plan.target_uid.expect("--uid-base always allocates a uid");
        let gid = plan.target_gid.expect("--uid-base always allocates a gid");
        program_args.push(format!("--uid={uid}"));
        program_args.push(format!("--gid={gid}"));
    }
    if let Some(uid) = args.uid {
        program_args.push(format!("--uid={uid}"));
    }
    if let Some(gid) = args.gid {
        program_args.push(format!("--gid={gid}"));
    }
    if let Some(ref user) = args.user {
        program_args.push(format!("--user={user}"));
    }
    if args.allow_shared_uid {
        // A KeepAlive restart must not start refusing where the original
        // launch was explicitly allowed to share.
        program_args.push("--allow-shared-uid".to_string());
    }
    if let Some(nofile) = args.rlimit_nofile {
        program_args.push(format!("--rlimit-nofile={nofile}"));
    }
    if let Some(nproc) = args.rlimit_nproc {
        program_args.push(format!("--rlimit-nproc={nproc}"));
    }
    if let Some(fsize) = args.rlimit_fsize {
        program_args.push(format!("--rlimit-fsize={fsize}"));
    }

    // launchd opens the log paths as root before each (re)start, so they
    // must NOT live inside the daemon-writable (chowned after a privilege
    // drop) work dir — a compromised daemon could symlink-swap them and have
    // root append to an arbitrary path. Keep them as root-owned dot-siblings
    // in the work root, like the profile and lock; the daemon still writes
    // to them via the fds launchd hands it. `--teardown` removes them.
    let work_root_abs =
        std::fs::canonicalize(&plan.work_root).unwrap_or_else(|_| plan.work_root.clone());
    let program_args_xml = program_args
        .iter()
        .map(|a| format!("        <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program_args_xml}
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{work_root}/.{id}.launchd.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{work_root}/.{id}.launchd.stderr.log</string>
    <key>ThrottleInterval</key>
    <integer>5</integer>
</dict>
</plist>
"#,
        label = label,
        program_args_xml = program_args_xml,
        work_root = xml_escape(&work_root_abs.to_string_lossy()),
        id = xml_escape(&args.id),
    ))
}

fn run(args: Args) -> Result<u8, JailerError> {
    if args.teardown {
        return teardown(&args);
    }
    // Resolve any explicit privilege-drop target up front: a `--user`
    // lookup goes through `getpwnam`, which may allocate, so it must happen
    // here in the parent — never inside the child's `pre_exec`. This also
    // fails fast on an unknown user before anything is touched on disk.
    // `--uid-base` allocation happens inside `prepare`, under the instance
    // lock; the final identity comes back in the plan either way.
    let explicit = resolve_user(args.user.as_deref(), args.uid, args.gid)
        .map_err(|source| JailerError::PrivilegeDrop { source })?;
    let plan = prepare(&args, explicit)?;

    if args.generate_launchd_plist || args.launchd_plist_path.is_some() {
        let plist = generate_launchd_plist(&args, &plan)?;
        if let Some(ref path) = args.launchd_plist_path {
            std::fs::write(path, &plist).map_err(|source| JailerError::WritePlist {
                path: path.clone(),
                source,
            })?;
            eprintln!(
                "hephaestus-jailer: wrote launchd plist to {}",
                path.display()
            );
        } else {
            print!("{plist}");
        }
        return Ok(0);
    }

    // Hand only the per-VM work dir to the drop target so it can create the
    // API socket, logs, metrics, and snapshots. The profile deliberately
    // remains owner/root-owned outside this writable directory.
    if plan.target_uid.is_some() || plan.target_gid.is_some() {
        std::os::unix::fs::chown(&plan.work_dir, plan.target_uid, plan.target_gid).map_err(
            |source| JailerError::Chown {
                path: plan.work_dir.clone(),
                source,
            },
        )?;
    }

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
    // Spawn (not `status()`) so we can forward termination signals: the child
    // is its own process-group leader, so a `SIGTERM`/`SIGINT` to the jailer
    // is relayed to the whole daemon+VM subtree instead of orphaning it.
    let mut child = cmd.spawn().map_err(|source| JailerError::Exec {
        binary: plan.binary.clone(),
        source,
    })?;
    CHILD_PGID.store(child.id().cast_signed(), Ordering::SeqCst);
    install_signal_forwarding();
    let status = child.wait().map_err(|source| JailerError::Exec {
        binary: plan.binary.clone(),
        source,
    })?;
    // The instance lock in `plan` must outlive the child: parent and child
    // share one open file description, so dropping the `Flock` earlier would
    // release the running daemon's claim too. It unlocks here, after wait.
    drop(plan);
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
/// guessable `<root>/<id>` for us to descend into. Forcing owner-only perms
/// fails closed (EPERM) if another user already owns the path.
///
/// When a privilege drop is requested the root gets `0711` instead of `0700`:
/// still owner-writable/listable only, but traversable, so the dropped-uid
/// daemon can reach its own (chowned) per-VM dir beneath it. With `0700` the
/// child would fail path traversal on everything under the root after setuid.
fn secure_work_root(root: &Path, drop_requested: bool) -> Result<(), JailerError> {
    if !assert_trusted_work_root(root)? {
        std::fs::create_dir_all(root).map_err(|source| JailerError::CreateWorkDir {
            path: root.to_path_buf(),
            source,
        })?;
    }
    // Enforce private perms on every run. If we don't own the directory this
    // fails with EPERM, which is the fail-closed outcome we want.
    let mode = if drop_requested { 0o711 } else { 0o700 };
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).map_err(|_| {
        JailerError::UnsafeWorkDir {
            path: root.to_path_buf(),
            reason: "cannot enforce private owner-only perms (not owner?)",
        }
    })
}

/// Refusal checks shared by launch (`secure_work_root`) and `--teardown`:
/// an existing root must be a non-symlink directory owned by the invoking
/// euid. Returns `Ok(false)` when the root does not exist, so teardown can
/// treat that as "nothing to remove" without creating it as a side effect.
fn assert_trusted_work_root(root: &Path) -> Result<bool, JailerError> {
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
            // Root can chmod directories owned by anyone, so permission
            // normalization alone does not prove ownership when the jailer is
            // privileged. Reject a pre-planted root owned by another uid.
            // SAFETY: geteuid has no preconditions and only reads process state.
            if meta.uid() != unsafe { libc::geteuid() } {
                return Err(JailerError::UnsafeWorkDir {
                    path: root.to_path_buf(),
                    reason: "work root is not owned by the invoking uid",
                });
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(JailerError::CreateWorkDir {
            path: root.to_path_buf(),
            source,
        }),
    }
}

/// Retire an instance's on-disk state: the per-VM work dir and the root-owned
/// dot-siblings (`.{id}.sandbox.profile`, `.{id}.launchd.*.log`, and last the
/// `.{id}.lock` file). Refuses while the instance is running — its flock is
/// held — and is idempotent when nothing exists. The lock file is unlinked
/// *before* the flock is released: a concurrent launch either still sees the
/// held lock or re-opens a fresh file, and `acquire_instance_lock`'s inode
/// re-check catches the window in between.
fn teardown(args: &Args) -> Result<u8, JailerError> {
    validate_id(&args.id)?;
    let work_root = args.work_dir.clone().unwrap_or_else(default_work_root);
    if !assert_trusted_work_root(&work_root)? {
        eprintln!(
            "hephaestus-jailer: nothing to remove under {}",
            work_root.display()
        );
        return Ok(0);
    }

    let lock_path = work_root.join(format!(".{}.lock", args.id));
    let lock = if std::fs::symlink_metadata(&lock_path).is_ok() {
        Some(acquire_instance_lock(&lock_path, &args.id)?)
    } else {
        None
    };

    let work_dir = work_root.join(&args.id);
    // Same leaf guard as launch: never descend into (or delete through) a
    // planted symlink at the exact per-VM path.
    if let Ok(meta) = std::fs::symlink_metadata(&work_dir)
        && meta.file_type().is_symlink()
    {
        return Err(JailerError::UnsafeWorkDir {
            path: work_dir,
            reason: "path is a symlink",
        });
    }

    let remove_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| JailerError::Remove { path, source }
    };
    if std::fs::symlink_metadata(&work_dir).is_ok() {
        std::fs::remove_dir_all(&work_dir).map_err(remove_err(&work_dir))?;
    }
    for name in [
        format!(".{}.sandbox.profile", args.id),
        format!(".{}.launchd.stdout.log", args.id),
        format!(".{}.launchd.stderr.log", args.id),
    ] {
        let path = work_root.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path).map_err(remove_err(&path))?;
        }
    }
    // Release the id's uid allocation. Guarded on the registry's existence:
    // `Registry::open` creates the file, and teardown of a never-launched
    // (or non-dropping) id must stay a pure no-op. Instance lock is still
    // held here, preserving the instance-before-registry lock order.
    if std::fs::symlink_metadata(work_root.join(".uid-allocations")).is_ok() {
        let mut reg = uid_registry::Registry::open(&work_root)?;
        reg.remove(&args.id)?;
    }
    if lock.is_some() && std::fs::symlink_metadata(&lock_path).is_ok() {
        std::fs::remove_file(&lock_path).map_err(remove_err(&lock_path))?;
    }
    drop(lock);

    eprintln!(
        "hephaestus-jailer: tore down instance {:?} under {}",
        args.id,
        work_root.display()
    );
    Ok(0)
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
            kernel: Some(touch(dir.join("vmlinux"))),
            rootfs: Some(touch(dir.join("rootfs.ext4"))),
            initramfs: None,
            pool_dir: None,
            deny_probe: None,
            rlimit_nofile: None,
            rlimit_nproc: None,
            rlimit_fsize: None,
            uid: None,
            gid: None,
            user: None,
            generate_launchd_plist: false,
            launchd_plist_path: None,
            clean_work_dir: false,
            teardown: false,
            uid_base: None,
            allow_shared_uid: false,
        }
    }

    /// `prepare` with the explicit target derived from the args, the way
    /// `run` does it (minus the `--user` passwd lookup, which tests avoid).
    fn prep(args: &Args) -> Result<Plan, JailerError> {
        prepare(args, (args.uid, args.gid))
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
        args.kernel = Some(dir.join("no-such-kernel"));
        assert!(matches!(
            prep(&args),
            Err(JailerError::KernelNotFound { .. })
        ));
    }

    #[test]
    fn prepare_rejects_missing_rootfs() {
        let dir = scratch("missing-rootfs");
        let mut args = args_in(&dir);
        args.rootfs = Some(dir.join("no-such-rootfs"));
        assert!(matches!(
            prep(&args),
            Err(JailerError::RootfsNotFound { .. })
        ));
    }

    #[test]
    fn prepare_rejects_missing_binary() {
        let dir = scratch("missing-binary");
        let mut args = args_in(&dir);
        args.firecracker_binary = Some(dir.join("no-such-binary"));
        assert!(matches!(
            prep(&args),
            Err(JailerError::BinaryNotFound { .. })
        ));
    }

    #[test]
    fn prepare_materializes_work_dir_and_profile() {
        let dir = scratch("happy");
        let args = args_in(&dir);
        let plan = prep(&args).expect("prepare should succeed");

        assert!(plan.work_dir.is_dir(), "work dir should be created");
        assert_eq!(plan.work_dir.file_name().unwrap(), "vm-test");
        assert_eq!(plan.api_sock, plan.work_dir.join("api.sock"));
        assert_eq!(
            plan.profile_path,
            plan.work_root.join(".vm-test.sandbox.profile")
        );
        assert!(
            !plan.profile_path.starts_with(&plan.work_dir),
            "the profile must stay outside the daemon-writable directory"
        );

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
        let plan = prep(&args).unwrap();
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
        let plan = prep(&args).unwrap();
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
        assert!(matches!(prep(&args), Err(JailerError::InvalidId { .. })));
    }

    #[test]
    fn resolve_user_with_explicit_uid_gid() {
        let (uid, gid) = resolve_user(None, Some(1000), Some(1000)).unwrap();
        assert_eq!(uid, Some(1000));
        assert_eq!(gid, Some(1000));
    }

    #[test]
    fn resolve_user_with_uid_only() {
        let (uid, gid) = resolve_user(None, Some(1001), None).unwrap();
        assert_eq!(uid, Some(1001));
        assert_eq!(gid, None);
    }

    #[test]
    fn resolve_user_with_gid_only() {
        let (uid, gid) = resolve_user(None, None, Some(1002)).unwrap();
        assert_eq!(uid, None);
        assert_eq!(gid, Some(1002));
    }

    #[test]
    fn resolve_user_with_none() {
        let (uid, gid) = resolve_user(None, None, None).unwrap();
        assert_eq!(uid, None);
        assert_eq!(gid, None);
    }

    #[test]
    fn resolve_user_lookup_existing_user() {
        // "nobody" exists on every macOS system.
        let (uid, gid) = resolve_user(Some("nobody"), None, None).unwrap();
        assert!(uid.is_some(), "nobody should have a uid");
        assert!(gid.is_some(), "nobody should have a gid");
    }

    #[test]
    fn resolve_user_lookup_nonexistent_user() {
        let err = resolve_user(Some("this-user-does-not-exist-42"), None, None);
        assert!(err.is_err(), "nonexistent user should produce an error");
    }

    #[test]
    fn args_user_conflicts_with_uid_and_gid() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["hephaestus-jailer", "--kernel", "k", "--rootfs", "r"];
            argv.extend_from_slice(extra);
            Args::try_parse_from(argv)
        };
        parse(&["--user", "nobody", "--uid", "1"]).unwrap_err();
        parse(&["--user", "nobody", "--gid", "1"]).unwrap_err();
        parse(&["--user", "nobody"]).unwrap();
        parse(&["--uid", "1", "--gid", "2"]).unwrap();
    }

    #[test]
    fn args_launchd_plist_path_implies_generation() {
        let parsed = Args::try_parse_from([
            "hephaestus-jailer",
            "--kernel",
            "k",
            "--rootfs",
            "r",
            "--launchd-plist-path",
            "/tmp/x.plist",
        ])
        .unwrap();
        assert!(
            !parsed.generate_launchd_plist && parsed.launchd_plist_path.is_some(),
            "path alone must parse; run() treats it as implying generation"
        );
    }

    #[test]
    fn prepare_makes_work_root_traversable_only_when_dropping_privileges() {
        let dir = scratch("privdrop-root-mode");
        let mut args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&plan.work_root), 0o700, "no drop → fully private root");
        assert_eq!(mode(&plan.work_dir), 0o700, "work dir stays private");
        assert_eq!(
            mode(&plan.profile_path),
            0o600,
            "profile is owner-readable without a drop"
        );

        // Release the first run's instance lock before re-preparing the
        // same id — a live Plan holds the flock and the second claim would
        // be refused as InstanceBusy.
        drop(plan);
        args.uid = Some(1);
        let plan = prep(&args).unwrap();
        assert_eq!(
            mode(&plan.work_root),
            0o711,
            "drop requested → root must be traversable for the dropped uid"
        );
        assert_eq!(mode(&plan.work_dir), 0o700, "work dir stays private");
        assert_eq!(
            mode(&plan.profile_path),
            0o644,
            "profile must be readable by the dropped uid"
        );
    }

    #[test]
    fn xml_escape_escapes_markup() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("/plain/path"), "/plain/path");
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

        let plan = prep(&args).unwrap();
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

    #[test]
    fn generate_launchd_plist_contains_label_and_keepalive() {
        let dir = scratch("launchd-plist");
        let args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        assert!(
            plist.contains("com.hephaestus.vm.vm-test"),
            "plist should contain the label"
        );
        assert!(plist.contains("RunAtLoad"), "plist should run at load");
        assert!(
            plist.contains("ThrottleInterval"),
            "plist should have a throttle interval"
        );
        assert!(plist.contains("KeepAlive"), "plist should have KeepAlive");
        assert!(
            plist.contains("<key>KeepAlive</key>\n    <true/>"),
            "launchd should restart the jailer after every daemon exit"
        );
    }

    #[test]
    fn generate_launchd_plist_includes_uid_gid_when_set() {
        let dir = scratch("launchd-plist-uid");
        let mut args = args_in(&dir);
        args.uid = Some(1001);
        args.gid = Some(1002);
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        assert!(plist.contains("--uid=1001"), "plist should include --uid");
        assert!(plist.contains("--gid=1002"), "plist should include --gid");
    }

    #[test]
    fn generate_launchd_plist_includes_user_when_set() {
        let dir = scratch("launchd-plist-user");
        let mut args = args_in(&dir);
        args.user = Some("nobody".into());
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        assert!(
            plist.contains("--user=nobody"),
            "plist should include --user"
        );
    }

    #[test]
    fn generate_launchd_plist_runs_the_jailer_not_the_daemon() {
        let dir = scratch("launchd-plist-argv0");
        let args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        // argv[0] must be this executable (the jailer), so launchd re-runs
        // the whole jail setup — not the bare daemon with jailer flags.
        let exe = std::env::current_exe().unwrap();
        assert!(
            plist.contains(&format!(
                "<string>{}</string>",
                xml_escape(&exe.to_string_lossy())
            )),
            "plist argv[0] should be the jailer executable"
        );
        // The resolved daemon path is pinned (canonicalized) so launchd
        // never re-does a $PATH lookup at load time.
        assert!(
            plist.contains(&format!(
                "--firecracker-binary={}",
                fs::canonicalize(&plan.binary).unwrap().to_string_lossy()
            )),
            "plist should pin the resolved daemon binary"
        );
    }

    #[test]
    fn prepare_refuses_second_claim_on_the_same_id() {
        let dir = scratch("lock-second-claim");
        let args = args_in(&dir);
        let first = prep(&args).expect("first claim should succeed");
        assert!(
            matches!(prep(&args), Err(JailerError::InstanceBusy { .. })),
            "a live instance must refuse a second jailer for the same id"
        );
        drop(first);
        prep(&args).expect("a released id should be claimable again");
    }

    #[test]
    fn prepare_removes_stale_api_sock() {
        let dir = scratch("stale-sock");
        let args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let sock = plan.api_sock.clone();
        drop(plan);
        // Stand-in for a dead daemon's leftover socket. (Binding a real UDS
        // here would exceed SUN_LEN on macOS's deep per-user temp paths;
        // removal goes through symlink_metadata + remove_file either way.)
        touch(sock.clone());
        assert!(sock.exists());
        let _plan = prep(&args).unwrap();
        assert!(
            std::fs::symlink_metadata(&sock).is_err(),
            "prepare should unlink a stale api socket"
        );
    }

    #[test]
    fn prepare_reuses_existing_work_dir_preserving_contents() {
        let dir = scratch("reuse-dir");
        let args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let snapshot = plan.work_dir.join("snapshot.bin");
        drop(plan);
        touch(snapshot.clone());
        fs::set_permissions(
            snapshot.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let plan = prep(&args).unwrap();
        assert!(
            snapshot.exists(),
            "restarts must preserve work-dir contents by default"
        );
        let mode = fs::metadata(&plan.work_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "work-dir mode is renormalized on every run");
    }

    #[test]
    fn clean_work_dir_empties_the_per_vm_dir() {
        let dir = scratch("clean-dir");
        let mut args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let stale = touch(plan.work_dir.join("snapshot.bin"));
        drop(plan);

        args.clean_work_dir = true;
        let plan = prep(&args).unwrap();
        assert!(plan.work_dir.is_dir(), "work dir is recreated");
        assert!(
            !stale.exists(),
            "--clean-work-dir must discard old contents"
        );
    }

    #[test]
    fn teardown_removes_work_dir_profile_and_lock() {
        let dir = scratch("teardown-happy");
        let mut args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let (work_dir, profile_path, work_root) = (
            plan.work_dir.clone(),
            plan.profile_path.clone(),
            plan.work_root.clone(),
        );
        drop(plan);

        args.teardown = true;
        assert_eq!(teardown(&args).unwrap(), 0);
        assert!(!work_dir.exists(), "work dir should be removed");
        assert!(!profile_path.exists(), "profile should be removed");
        assert!(
            !work_root.join(".vm-test.lock").exists(),
            "lock file should be removed"
        );
    }

    #[test]
    fn teardown_refuses_while_the_instance_is_running() {
        let dir = scratch("teardown-busy");
        let mut args = args_in(&dir);
        let plan = prep(&args).unwrap();
        args.teardown = true;
        assert!(
            matches!(teardown(&args), Err(JailerError::InstanceBusy { .. })),
            "teardown must refuse while the instance lock is held"
        );
        assert!(plan.work_dir.is_dir(), "a running instance keeps its state");
    }

    #[test]
    fn teardown_refuses_a_symlinked_work_dir() {
        let dir = scratch("teardown-symlink");
        let mut args = args_in(&dir);
        let root = dir.join("work");
        fs::create_dir_all(&root).unwrap();
        let victim = dir.join("victim");
        fs::create_dir_all(&victim).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("vm-test")).unwrap();

        args.teardown = true;
        assert!(
            matches!(teardown(&args), Err(JailerError::UnsafeWorkDir { .. })),
            "teardown must not delete through a planted symlink"
        );
        assert!(victim.exists(), "the symlink target must be untouched");
    }

    #[test]
    fn teardown_is_idempotent_when_nothing_exists() {
        let dir = scratch("teardown-idempotent");
        let mut args = args_in(&dir);
        args.work_dir = Some(dir.join("never-created"));
        args.teardown = true;
        assert_eq!(teardown(&args).unwrap(), 0, "missing root → nothing to do");
        assert_eq!(teardown(&args).unwrap(), 0, "and it stays repeatable");
    }

    #[test]
    fn secure_work_root_rejects_symlink_and_non_directory_roots() {
        let dir = scratch("root-rejects");
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = dir.join("link-root");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            secure_work_root(&link, false),
            Err(JailerError::UnsafeWorkDir {
                reason: "work root is a symlink",
                ..
            })
        ));

        let file = touch(dir.join("file-root"));
        assert!(matches!(
            secure_work_root(&file, false),
            Err(JailerError::UnsafeWorkDir {
                reason: "work root exists but is not a directory",
                ..
            })
        ));
    }

    #[test]
    fn args_teardown_waives_kernel_and_rootfs() {
        Args::try_parse_from(["hephaestus-jailer", "--teardown"]).unwrap();
        Args::try_parse_from(["hephaestus-jailer"]).unwrap_err();
        Args::try_parse_from(["hephaestus-jailer", "--kernel", "k"]).unwrap_err();
    }

    #[test]
    fn args_teardown_conflicts_with_plist_and_clean() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["hephaestus-jailer", "--teardown"];
            argv.extend_from_slice(extra);
            Args::try_parse_from(argv)
        };
        parse(&["--generate-launchd-plist"]).unwrap_err();
        parse(&["--launchd-plist-path", "/tmp/x.plist"]).unwrap_err();
        parse(&["--clean-work-dir"]).unwrap_err();
    }

    #[test]
    fn launchd_plist_logs_live_outside_the_chowned_work_dir() {
        let dir = scratch("launchd-log-paths");
        let args = args_in(&dir);
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        let root = fs::canonicalize(&plan.work_root).unwrap();
        for stream in ["stdout", "stderr"] {
            assert!(
                plist.contains(&format!(
                    "<string>{}/.vm-test.launchd.{stream}.log</string>",
                    root.to_string_lossy()
                )),
                "launchd {stream} log must be a root-owned work-root sibling:\n{plist}"
            );
        }
        assert!(
            !plist.contains("vm-test/launchd."),
            "launchd logs must not live inside the daemon-writable work dir"
        );
    }

    #[test]
    fn launchd_plist_never_carries_clean_work_dir() {
        let dir = scratch("launchd-no-clean");
        let mut args = args_in(&dir);
        // clap allows --clean-work-dir alongside plist generation (it only
        // conflicts with --teardown), so the builder itself must drop it.
        args.clean_work_dir = true;
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();
        assert!(
            !plist.contains("--clean-work-dir"),
            "a KeepAlive plist carrying --clean-work-dir would wipe snapshots on every restart"
        );
    }

    #[test]
    fn args_uid_base_conflicts_with_explicit_identity_flags() {
        let parse = |extra: &[&str]| {
            let mut argv = vec![
                "hephaestus-jailer",
                "--kernel",
                "k",
                "--rootfs",
                "r",
                "--uid-base",
                "61000",
            ];
            argv.extend_from_slice(extra);
            Args::try_parse_from(argv)
        };
        parse(&[]).unwrap();
        parse(&["--allow-shared-uid"]).unwrap();
        parse(&["--uid", "1"]).unwrap_err();
        parse(&["--gid", "1"]).unwrap_err();
        parse(&["--user", "nobody"]).unwrap_err();
    }

    #[test]
    fn uid_base_allocates_and_registers_a_dedicated_identity() {
        let dir = scratch("uid-alloc");
        let mut args = args_in(&dir);
        args.uid_base = Some(61000);
        let plan = prep(&args).unwrap();

        assert_eq!(plan.target_uid, Some(61000));
        assert_eq!(
            plan.target_gid,
            Some(61000),
            "gid follows the allocated uid"
        );
        let registry = fs::read_to_string(plan.work_root.join(".uid-allocations")).unwrap();
        assert!(
            registry.contains("vm-test 61000"),
            "entry recorded: {registry}"
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&plan.work_root.join(".uid-allocations")), 0o600);
        // --uid-base alone counts as a privilege drop for the mode split.
        assert_eq!(mode(&plan.work_root), 0o711);
        assert_eq!(mode(&plan.profile_path), 0o644);
    }

    #[test]
    fn uid_base_allocation_is_stable_across_relaunches() {
        let dir = scratch("uid-stable");
        let mut args = args_in(&dir);
        args.uid_base = Some(61000);
        let plan = prep(&args).unwrap();
        let first = plan.target_uid;
        drop(plan);
        let plan = prep(&args).unwrap();
        assert_eq!(
            plan.target_uid, first,
            "an id keeps its uid across restarts"
        );
    }

    #[test]
    fn two_ids_get_distinct_uids() {
        let dir = scratch("uid-distinct");
        let mut a = args_in(&dir);
        a.uid_base = Some(61000);
        let mut b = args_in(&dir);
        b.id = "vm-test-2".into();
        b.uid_base = Some(61000);

        let plan_a = prep(&a).unwrap();
        let plan_b = prep(&b).unwrap();
        assert_eq!(plan_a.target_uid, Some(61000));
        assert_eq!(
            plan_b.target_uid,
            Some(61001),
            "sibling gets the next free uid"
        );
    }

    #[test]
    fn live_shared_uid_is_refused_and_allow_flag_overrides() {
        let dir = scratch("uid-shared-live");
        let mut a = args_in(&dir);
        a.uid = Some(61000);
        let _live = prep(&a).expect("first instance claims the uid");

        let mut b = args_in(&dir);
        b.id = "vm-test-2".into();
        b.uid = Some(61000);
        let err = prep(&b)
            .err()
            .expect("second live same-uid launch must fail");
        match err {
            JailerError::SharedUidLive { id, other_id, uid } => {
                assert_eq!(id, "vm-test-2");
                assert_eq!(other_id, "vm-test");
                assert_eq!(uid, 61000);
            }
            other => panic!("expected SharedUidLive, got {other}"),
        }
        b.allow_shared_uid = true;
        prep(&b).expect("--allow-shared-uid overrides the refusal");
    }

    #[test]
    fn dead_instance_does_not_refuse_its_uid() {
        let dir = scratch("uid-shared-dead");
        let mut a = args_in(&dir);
        a.uid = Some(61000);
        drop(prep(&a).unwrap());

        let mut b = args_in(&dir);
        b.id = "vm-test-2".into();
        b.uid = Some(61000);
        prep(&b).expect("a stale registry entry with no held lock is not a conflict");
    }

    #[test]
    fn allocation_skips_a_uid_taken_by_an_explicit_instance() {
        let dir = scratch("uid-cross-mode");
        let mut a = args_in(&dir);
        a.uid = Some(61000);
        let _live = prep(&a).unwrap();

        let mut b = args_in(&dir);
        b.id = "vm-test-2".into();
        b.uid_base = Some(61000);
        let plan = prep(&b).unwrap();
        assert_eq!(
            plan.target_uid,
            Some(61001),
            "allocation must not collide with an explicit-uid instance"
        );
    }

    #[test]
    fn teardown_removes_only_this_ids_registry_entry() {
        let dir = scratch("uid-teardown");
        let mut a = args_in(&dir);
        a.uid_base = Some(61000);
        drop(prep(&a).unwrap());
        let mut b = args_in(&dir);
        b.id = "vm-test-2".into();
        b.uid_base = Some(61000);
        drop(prep(&b).unwrap());

        a.teardown = true;
        assert_eq!(teardown(&a).unwrap(), 0);
        let registry_path = a.work_dir.as_ref().unwrap().join(".uid-allocations");
        let registry = fs::read_to_string(&registry_path).unwrap();
        assert!(
            !registry.contains("vm-test "),
            "torn-down entry gone: {registry}"
        );
        assert!(
            registry.contains("vm-test-2 "),
            "sibling entry survives: {registry}"
        );
    }

    #[test]
    fn gid_only_drop_creates_no_registry() {
        let dir = scratch("gid-only");
        let mut args = args_in(&dir);
        args.gid = Some(61000);
        let plan = prep(&args).unwrap();
        assert!(
            !plan.work_root.join(".uid-allocations").exists(),
            "a gid-only drop has no uid to register"
        );
    }

    #[test]
    fn launchd_plist_pins_allocated_uid_and_omits_uid_base() {
        let dir = scratch("uid-plist");
        let mut args = args_in(&dir);
        args.uid_base = Some(61000);
        args.allow_shared_uid = true;
        let plan = prep(&args).unwrap();
        let plist = generate_launchd_plist(&args, &plan).unwrap();

        assert!(
            plist.contains("--uid=61000"),
            "allocated uid pinned:\n{plist}"
        );
        assert!(
            plist.contains("--gid=61000"),
            "allocated gid pinned:\n{plist}"
        );
        assert!(
            !plist.contains("--uid-base"),
            "restarts must not re-run allocation:\n{plist}"
        );
        assert!(
            plist.contains("--allow-shared-uid"),
            "a KeepAlive restart must not fail where the launch was allowed:\n{plist}"
        );
    }
}

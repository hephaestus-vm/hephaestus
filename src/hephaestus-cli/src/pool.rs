//! Warm-pool primitive.
//!
//! A pool is a directory of pre-warmed VZ snapshots that `hephaestus pool
//! run` claims a slot from, restores, runs a command inside, and releases.
//! Slot claims are exclusive via `flock(2)`; kernel cleanup on process
//! death means no stale-lock recovery is needed.
//!
//! Layout:
//! ```text
//! <pool-dir>/
//!   meta             # key=value: kernel, initramfs, pristine, save, slots, cpus, memory_mib
//!   pristine.ext4    # rootfs bytes matching the snapshot's disk state
//!   save.bin         # VZ machine-state snapshot
//!   save.machineid   # paired VZ machine identifier (managed by vz-warm save)
//!   slot-{0..N-1}/
//!     lock           # flock target
//!     rootfs.ext4    # APFS-CoW clone of pristine, present only while claimed
//! ```
//!
//! Deliberately kept framework-agnostic: `try_claim_slot` never blocks,
//! never errors "on full"; callers (CLI / future HTTP daemon) decide how
//! to react to a miss (error-first for drop-in Firecracker semantics, or
//! queue / fall back to cold boot).

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use hephaestus_vmm::{vz_exec_snapshot_restore, vz_exec_snapshot_save};
use nix::fcntl::{Flock, FlockArg};

/// Pool state persisted to `<dir>/meta`.
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub pristine: PathBuf,
    pub save: PathBuf,
    pub slots: u32,
    pub cpus: u32,
    pub memory_mib: u64,
}

/// A claimed slot. Drop releases the exclusive lock (kernel cleans up the
/// flock when the file handle closes). Drop also best-effort-removes the
/// slot's rootfs clone so the slot becomes "ready" for the next claimant.
pub struct ClaimedSlot {
    pub index: u32,
    /// Slot directory on disk. Kept for future consumers (e.g., the
    /// Firecracker-compat HTTP daemon) that want to introspect a claim —
    /// the CLI doesn't use it today.
    #[allow(dead_code)]
    pub path: PathBuf,
    pub rootfs: PathBuf,
    /// Held as long as we own the slot. Dropping this struct releases the
    /// flock and closes the fd.
    _lock: Flock<File>,
}

#[derive(Debug)]
pub struct Pool {
    pub dir: PathBuf,
    pub meta: PoolMeta,
}

/// Per-slot state reported by `Pool::stats`.
#[derive(Debug)]
pub struct SlotState {
    pub index: u32,
    /// True if the lock is currently held by another process.
    pub busy: bool,
    /// True if a rootfs clone is lingering (hint that something crashed
    /// mid-run before cleanup).
    pub has_rootfs: bool,
}

impl Pool {
    /// Build a fresh pool: clone rootfs, warm up + save, create empty
    /// slot directories.
    ///
    /// Refuses to overwrite an existing pool; caller should call
    /// [`Pool::destroy`] first if they want a clean slate.
    pub fn init(
        dir: &Path,
        kernel: &Path,
        initramfs: &Path,
        rootfs_source: &Path,
        slots: u32,
        cpus: u32,
        memory_mib: u64,
    ) -> Result<Self, PoolError> {
        if dir.exists() {
            return Err(PoolError::AlreadyExists(dir.to_path_buf()));
        }
        if slots == 0 {
            return Err(PoolError::InvalidSize);
        }
        fs::create_dir_all(dir).map_err(|e| PoolError::Io("create pool dir", e))?;
        let dir = dir.canonicalize().map_err(|e| PoolError::Io("canonicalize pool dir", e))?;

        let pristine = dir.join("pristine.ext4");
        let save = dir.join("save.bin");

        // Clone the source rootfs into the pool. cp -c = APFS CoW, so
        // this is near-instant for any size.
        clone_file(rootfs_source, &pristine)?;

        // Warm the VM against the cloned rootfs and persist the VZ state.
        // This mutates pristine.ext4 — that's intentional. After save,
        // pristine.ext4 is the reference disk state the snapshot refers
        // to, and every slot's rootfs will be a clone of it.
        vz_exec_snapshot_save(kernel, initramfs, &pristine, &save, None, cpus, memory_mib)
            .map_err(PoolError::VmSave)?;

        // Empty slot directories, each holding just a lock file.
        for i in 0..slots {
            let slot = dir.join(format!("slot-{i}"));
            fs::create_dir(&slot).map_err(|e| PoolError::Io("create slot dir", e))?;
            File::create(slot.join("lock"))
                .map_err(|e| PoolError::Io("create slot lock", e))?;
        }

        let meta = PoolMeta {
            kernel: kernel.canonicalize().unwrap_or_else(|_| kernel.into()),
            initramfs: initramfs.canonicalize().unwrap_or_else(|_| initramfs.into()),
            pristine,
            save,
            slots,
            cpus,
            memory_mib,
        };
        meta.write(&dir.join("meta"))?;

        Ok(Pool { dir, meta })
    }

    /// Load an existing pool from disk.
    pub fn open(dir: &Path) -> Result<Self, PoolError> {
        let dir = dir.canonicalize().map_err(|e| PoolError::Io("canonicalize pool dir", e))?;
        let meta_path = dir.join("meta");
        if !meta_path.exists() {
            return Err(PoolError::NotAPool(dir));
        }
        let meta = PoolMeta::read(&meta_path)?;
        Ok(Pool { dir, meta })
    }

    /// Non-blocking claim. Walks slots in order and returns the first
    /// that `flock(LOCK_EX | LOCK_NB)` accepts. `Ok(None)` means all
    /// slots are currently held by other processes.
    pub fn try_claim_slot(&self) -> Result<Option<ClaimedSlot>, PoolError> {
        for i in 0..self.meta.slots {
            let slot_path = self.dir.join(format!("slot-{i}"));
            let lock_path = slot_path.join("lock");
            let lock_file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| PoolError::Io("open slot lock", e))?;

            match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => {
                    return Ok(Some(ClaimedSlot {
                        index: i,
                        rootfs: slot_path.join("rootfs.ext4"),
                        path: slot_path,
                        _lock: lock,
                    }));
                }
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => continue,
                Err((_, errno)) => {
                    return Err(PoolError::FlockErrno(i, errno));
                }
            }
        }
        Ok(None)
    }

    /// Non-locking enumeration of slot state. Useful for `pool stats`.
    /// The `busy` signal is itself racy (a slot could be released the
    /// moment we return), which is fine — stats is a hint.
    pub fn stats(&self) -> Result<Vec<SlotState>, PoolError> {
        let mut out = Vec::with_capacity(self.meta.slots as usize);
        for i in 0..self.meta.slots {
            let slot_path = self.dir.join(format!("slot-{i}"));
            let lock_path = slot_path.join("lock");
            let rootfs_path = slot_path.join("rootfs.ext4");
            let has_rootfs = rootfs_path.exists();

            // Probe the flock non-blockingly, then immediately release.
            let busy = match File::options().read(true).write(true).open(&lock_path) {
                Ok(f) => match Flock::lock(f, FlockArg::LockExclusiveNonblock) {
                    Ok(_lock) => false, // got it → was free (and we just re-released)
                    Err((_, nix::errno::Errno::EWOULDBLOCK)) => true,
                    Err((_, errno)) => return Err(PoolError::FlockErrno(i, errno)),
                },
                Err(e) => return Err(PoolError::Io("open slot lock for stats", e)),
            };
            out.push(SlotState { index: i, busy, has_rootfs });
        }
        Ok(out)
    }

    /// Run `command` inside a claimed slot and return its exit code.
    ///
    /// `log` is optional; when `None` the guest serial output is written
    /// to a per-slot temp file that's deleted on success (callers who
    /// want to see guest output should pass `Some`).
    pub fn run(&self, slot: &ClaimedSlot, command: &str) -> Result<i32, PoolError> {
        // Clone pristine → slot rootfs for this run only.
        clone_file(&self.meta.pristine, &slot.rootfs)?;

        let log_owned: PathBuf;
        let log = match std::env::var_os("HEPHAESTUS_POOL_LOG") {
            Some(p) => {
                log_owned = PathBuf::from(p);
                Some(log_owned.as_path())
            }
            None => None,
        };

        let result = vz_exec_snapshot_restore(
            &self.meta.kernel,
            &self.meta.initramfs,
            &slot.rootfs,
            &self.meta.save,
            command,
            log,
            self.meta.cpus,
            self.meta.memory_mib,
        );

        // Whether the run succeeded or not, ditch the per-run rootfs so
        // the slot returns to "ready" when the lock releases.
        let _ = fs::remove_file(&slot.rootfs);

        result.map(|(code, _nanos)| code).map_err(PoolError::VmRestore)
    }

    /// Remove every file/dir under `dir` and the pool dir itself. Safe
    /// to call on a missing dir.
    pub fn destroy(dir: &Path) -> Result<(), PoolError> {
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(dir).map_err(|e| PoolError::Io("remove pool dir", e))
    }
}

impl Drop for ClaimedSlot {
    fn drop(&mut self) {
        // Best-effort cleanup in case `Pool::run` wasn't what removed
        // the rootfs (e.g., caller used the slot directly). The kernel
        // releases the flock when the File inside `_lock` closes.
        let _ = fs::remove_file(&self.rootfs);
    }
}

// =============================================================================
// Metadata persistence.
// =============================================================================

impl PoolMeta {
    fn write(&self, path: &Path) -> Result<(), PoolError> {
        let mut f = File::create(path).map_err(|e| PoolError::Io("write meta", e))?;
        writeln!(f, "kernel={}", self.kernel.display())
            .and_then(|_| writeln!(f, "initramfs={}", self.initramfs.display()))
            .and_then(|_| writeln!(f, "pristine={}", self.pristine.display()))
            .and_then(|_| writeln!(f, "save={}", self.save.display()))
            .and_then(|_| writeln!(f, "slots={}", self.slots))
            .and_then(|_| writeln!(f, "cpus={}", self.cpus))
            .and_then(|_| writeln!(f, "memory_mib={}", self.memory_mib))
            .map_err(|e| PoolError::Io("write meta body", e))?;
        Ok(())
    }

    fn read(path: &Path) -> Result<Self, PoolError> {
        let f = File::open(path).map_err(|e| PoolError::Io("open meta", e))?;
        let mut m = HashMap::<String, String>::new();
        for line in BufReader::new(f).lines() {
            let line = line.map_err(|e| PoolError::Io("read meta line", e))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| PoolError::BadMeta(format!("missing `=` in {line:?}")))?;
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
        let get = |k: &str| -> Result<String, PoolError> {
            m.get(k)
                .cloned()
                .ok_or_else(|| PoolError::BadMeta(format!("missing key `{k}`")))
        };
        let parse_u32 = |k: &str| -> Result<u32, PoolError> {
            get(k)?
                .parse()
                .map_err(|e| PoolError::BadMeta(format!("`{k}` not u32: {e}")))
        };
        let parse_u64 = |k: &str| -> Result<u64, PoolError> {
            get(k)?
                .parse()
                .map_err(|e| PoolError::BadMeta(format!("`{k}` not u64: {e}")))
        };
        Ok(PoolMeta {
            kernel: PathBuf::from(get("kernel")?),
            initramfs: PathBuf::from(get("initramfs")?),
            pristine: PathBuf::from(get("pristine")?),
            save: PathBuf::from(get("save")?),
            slots: parse_u32("slots")?,
            cpus: parse_u32("cpus")?,
            memory_mib: parse_u64("memory_mib")?,
        })
    }
}

// =============================================================================
// APFS clone via cp -c (standard on macOS). Falls back to a regular copy
// on filesystems that don't support clonefile, which means a slow but
// correct copy rather than a hard failure.
// =============================================================================

fn clone_file(src: &Path, dst: &Path) -> Result<(), PoolError> {
    // cp -c requests APFS clonefile; on APFS-to-APFS this is O(1) and
    // basically free. On other FSes macOS cp silently falls back to
    // a regular copy.
    let status = Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .status()
        .map_err(|e| PoolError::Io("spawn cp", e))?;
    if !status.success() {
        return Err(PoolError::CpFailed(status));
    }
    Ok(())
}

// =============================================================================
// Error type. Kept local to keep the cli crate dep-free beyond nix.
// =============================================================================

#[derive(Debug)]
pub enum PoolError {
    AlreadyExists(PathBuf),
    NotAPool(PathBuf),
    InvalidSize,
    Io(&'static str, std::io::Error),
    FlockErrno(u32, nix::errno::Errno),
    BadMeta(String),
    CpFailed(std::process::ExitStatus),
    VmSave(hephaestus_vmm::VmError),
    VmRestore(hephaestus_vmm::VmError),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::AlreadyExists(p) => write!(f, "pool dir already exists: {}", p.display()),
            PoolError::NotAPool(p) => write!(f, "not a pool dir (no meta): {}", p.display()),
            PoolError::InvalidSize => write!(f, "--size must be ≥ 1"),
            PoolError::Io(ctx, e) => write!(f, "{ctx}: {e}"),
            PoolError::FlockErrno(i, e) => write!(f, "flock on slot {i}: {e}"),
            PoolError::BadMeta(s) => write!(f, "bad pool meta: {s}"),
            PoolError::CpFailed(s) => write!(f, "cp -c exited {s}"),
            PoolError::VmSave(e) => write!(f, "vm save: {e}"),
            PoolError::VmRestore(e) => write!(f, "vm restore: {e}"),
        }
    }
}

impl std::error::Error for PoolError {}

// =============================================================================
// Tests
//
// The heavy lifting — `Pool::init` and `Pool::run` — needs a live VZ and
// the guest agent, so it's covered by integration tests (the `just pool-*`
// recipes). These unit tests exercise the pieces that DON'T need a VM:
// the meta file format, the flock-based claim dance, and stats. They
// build a "fake pool" directory with empty pristine/save/lock files so
// `Pool::try_claim_slot` and `Pool::stats` have real disk to work against
// without booting anything.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A test-unique temp directory. Cargo runs unit tests in parallel by
    /// default, so baking the test name into the path avoids collisions.
    fn tmp_for(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hh-pool-test-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a minimally-valid pool dir without booting any VM. The
    /// pristine / save files are empty placeholders because claim and
    /// stats don't read them.
    fn make_fake_pool(d: &Path, slots: u32) -> Pool {
        fs::write(d.join("pristine.ext4"), b"").unwrap();
        fs::write(d.join("save.bin"), b"").unwrap();
        let meta = PoolMeta {
            kernel: PathBuf::from("/k"),
            initramfs: PathBuf::from("/i"),
            pristine: d.join("pristine.ext4"),
            save: d.join("save.bin"),
            slots,
            cpus: 2,
            memory_mib: 512,
        };
        meta.write(&d.join("meta")).unwrap();
        for i in 0..slots {
            let s = d.join(format!("slot-{i}"));
            fs::create_dir(&s).unwrap();
            File::create(s.join("lock")).unwrap();
        }
        Pool::open(d).unwrap()
    }

    #[test]
    fn meta_roundtrip_preserves_all_fields() {
        let d = tmp_for("meta_roundtrip");
        let meta = PoolMeta {
            kernel: PathBuf::from("/path/to/kernel"),
            initramfs: PathBuf::from("/path/to/initramfs.cpio.gz"),
            pristine: PathBuf::from("/path/to/pristine.ext4"),
            save: PathBuf::from("/path/to/save.bin"),
            slots: 7,
            cpus: 3,
            memory_mib: 1024,
        };
        meta.write(&d.join("meta")).unwrap();
        let back = PoolMeta::read(&d.join("meta")).unwrap();
        assert_eq!(back.kernel, meta.kernel);
        assert_eq!(back.initramfs, meta.initramfs);
        assert_eq!(back.pristine, meta.pristine);
        assert_eq!(back.save, meta.save);
        assert_eq!(back.slots, meta.slots);
        assert_eq!(back.cpus, meta.cpus);
        assert_eq!(back.memory_mib, meta.memory_mib);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn meta_read_rejects_missing_key() {
        let d = tmp_for("meta_missing");
        fs::write(d.join("meta"), "kernel=/k\n").unwrap();
        match PoolMeta::read(&d.join("meta")) {
            Err(PoolError::BadMeta(msg)) => assert!(msg.contains("missing key")),
            other => panic!("expected BadMeta, got {other:?}"),
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn meta_read_rejects_non_integer_slot_count() {
        let d = tmp_for("meta_bad_int");
        fs::write(
            d.join("meta"),
            "kernel=/k\ninitramfs=/i\npristine=/p\nsave=/s\nslots=not_a_number\ncpus=1\nmemory_mib=1\n",
        )
        .unwrap();
        match PoolMeta::read(&d.join("meta")) {
            Err(PoolError::BadMeta(msg)) => assert!(msg.contains("slots")),
            other => panic!("expected BadMeta, got {other:?}"),
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn meta_read_ignores_comments_and_blank_lines() {
        let d = tmp_for("meta_comments");
        fs::write(
            d.join("meta"),
            "\
# this is a comment
kernel=/k
initramfs=/i

pristine=/p
save=/s
# another comment
slots=2
cpus=1
memory_mib=128
",
        )
        .unwrap();
        let meta = PoolMeta::read(&d.join("meta")).unwrap();
        assert_eq!(meta.slots, 2);
        assert_eq!(meta.memory_mib, 128);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn destroy_on_missing_dir_is_idempotent() {
        let d = std::env::temp_dir().join(format!(
            "hh-pool-test-{}-nonexistent-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!d.exists());
        Pool::destroy(&d).expect("destroy on missing dir should not error");
    }

    #[test]
    fn open_non_pool_dir_errors_clearly() {
        let d = tmp_for("not_a_pool");
        // Dir exists but has no `meta` file.
        match Pool::open(&d) {
            Err(PoolError::NotAPool(_)) => {}
            other => panic!("expected NotAPool, got {other:?}"),
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn claim_two_distinct_slots_when_both_free() {
        let d = tmp_for("claim_two");
        let pool = make_fake_pool(&d, 2);
        let c1 = pool.try_claim_slot().unwrap().expect("first claim");
        let c2 = pool.try_claim_slot().unwrap().expect("second claim");
        assert_ne!(c1.index, c2.index, "should pick distinct slots");
        drop(c1);
        drop(c2);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn claim_returns_none_when_all_slots_held() {
        let d = tmp_for("claim_exhausted");
        let pool = make_fake_pool(&d, 2);
        let _c1 = pool.try_claim_slot().unwrap().expect("c1");
        let _c2 = pool.try_claim_slot().unwrap().expect("c2");
        let c3 = pool.try_claim_slot().unwrap();
        assert!(c3.is_none(), "third claim should miss when 2/2 are held");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn claim_reusable_after_drop() {
        let d = tmp_for("claim_reuse");
        let pool = make_fake_pool(&d, 1);
        {
            let c = pool.try_claim_slot().unwrap().expect("first claim");
            drop(c);
        }
        let again = pool.try_claim_slot().unwrap().expect("should re-claim");
        assert_eq!(again.index, 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stats_reflects_claim_and_release() {
        let d = tmp_for("stats");
        let pool = make_fake_pool(&d, 3);
        let before = pool.stats().unwrap();
        assert_eq!(before.iter().filter(|s| s.busy).count(), 0);

        let claim = pool.try_claim_slot().unwrap().expect("claim");
        let during = pool.stats().unwrap();
        assert_eq!(
            during.iter().filter(|s| s.busy).count(),
            1,
            "exactly one slot should read busy while held"
        );
        assert!(during[claim.index as usize].busy);

        drop(claim);
        let after = pool.stats().unwrap();
        assert_eq!(after.iter().filter(|s| s.busy).count(), 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn drop_cleans_up_rootfs_file() {
        let d = tmp_for("drop_cleanup");
        let pool = make_fake_pool(&d, 1);
        let claim = pool.try_claim_slot().unwrap().unwrap();
        // Simulate a run by touching the per-slot rootfs.
        fs::write(&claim.rootfs, b"pretend this is an ext4").unwrap();
        assert!(claim.rootfs.exists());
        drop(claim);
        assert!(!pool.dir.join("slot-0/rootfs.ext4").exists(),
            "ClaimedSlot::drop should remove the rootfs clone");
        fs::remove_dir_all(&d).unwrap();
    }
}

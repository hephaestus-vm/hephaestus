//! Per-instance uid allocation registry.
//!
//! One root-owned file, `<work-root>/.uid-allocations`, records which uid
//! each instance id runs as: one `<id> <uid>` line per instance. It backs
//! `--uid-base` allocation (each id gets, and keeps across restarts, its
//! own uid) and the live-shared-uid refusal, which needs to know every
//! instance's uid regardless of how it was chosen — explicit `--uid` and
//! `--user` launches register here too.
//!
//! Mutations are serialized by a blocking exclusive `flock` on the registry
//! file itself. Unlike the per-instance lock in `main.rs`, this file is
//! never unlinked (teardown removes an id's *line*, not the file), so the
//! open needs no inode re-verification against a concurrent unlink.
//!
//! The registry is advisory bookkeeping, not a security boundary: it lives
//! outside every daemon-writable directory, and losing it is recoverable —
//! launchd plists pin their allocated uids and explicit launches re-register
//! on every run, so entries self-heal.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use thiserror::Error;

/// Name of the registry file inside the work root. A dot-sibling like the
/// profile and lock files; it can never collide with a per-id dot file
/// because the id charset excludes `.`.
const REGISTRY_NAME: &str = ".uid-allocations";

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("failed to open uid registry {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to lock uid registry {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "corrupt uid registry {}: bad line {line:?} (the file is safe to delete: \
         launchd plists pin their uids and explicit launches re-register)",
        path.display()
    )]
    Corrupt { path: PathBuf, line: String },
    #[error("failed to rewrite uid registry {}: {source}", path.display())]
    Rewrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "no free uid for instance {id:?} in [{base}, {end}): every uid is taken by a \
         registry entry or an existing user account (tear down retired instances with \
         --teardown to release their uids)"
    )]
    Exhausted {
        id: String,
        base: libc::uid_t,
        end: libc::uid_t,
    },
}

/// The registry, held open and exclusively flocked for the caller's whole
/// critical section (allocate/upsert/probe/remove happen under one lock).
pub struct Registry {
    file: Flock<File>,
    path: PathBuf,
    entries: Vec<(String, libc::uid_t)>,
}

/// Same charset rule as the jailer's `validate_id`: ids read back from disk
/// must stay single safe path components before anyone joins them into a
/// `.{id}.lock` path. Re-checked here so a hand-edited or corrupt registry
/// fails closed instead of traversing.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Registry {
    /// Open (creating if absent) and exclusively lock the work root's
    /// registry, then parse it. Blocking lock: critical sections are a
    /// read-and-rewrite of a tiny file, so contention is momentary.
    pub fn open(work_root: &Path) -> Result<Registry, RegistryError> {
        let path = work_root.join(REGISTRY_NAME);
        let open_err = |source| RegistryError::Open {
            path: path.clone(),
            source,
        };
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            // Created 0600 atomically — never even momentarily wider under
            // the 0711-traversable work root of a privilege-drop deployment.
            .mode(0o600)
            .open(&path)
            .map_err(open_err)?;
        // Normalize a pre-existing file too (mode() only applies at create).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(open_err)?;
        let mut file = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| {
            RegistryError::Lock {
                path: path.clone(),
                source: std::io::Error::from_raw_os_error(errno as i32),
            }
        })?;

        let mut raw = String::new();
        file.read_to_string(&mut raw).map_err(open_err)?;
        let mut entries = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parsed = line.split_once(' ').and_then(|(id, uid)| {
                if !valid_id(id) {
                    return None;
                }
                uid.parse::<libc::uid_t>().ok().map(|u| (id.to_string(), u))
            });
            match parsed {
                Some(entry) => entries.push(entry),
                None => {
                    return Err(RegistryError::Corrupt {
                        path,
                        line: line.to_string(),
                    });
                }
            }
        }
        Ok(Registry {
            file,
            path,
            entries,
        })
    }

    /// The parsed `(id, uid)` entries, for the caller's shared-uid probe.
    pub fn entries(&self) -> &[(String, libc::uid_t)] {
        &self.entries
    }

    /// Choose a uid for `id`: an existing entry is reused verbatim (the id
    /// may own previously chowned files, and a launchd restart must land on
    /// the same uid), otherwise the lowest uid in `[base, base+range)` free
    /// of both registry entries — even dead ones, which may be pinned into
    /// a sibling's launchd plist — and existing user accounts, as reported
    /// by the injected `passwd_taken` (injected so tests need no root and
    /// no real accounts).
    pub fn allocate(
        &self,
        id: &str,
        base: libc::uid_t,
        range: libc::uid_t,
        passwd_taken: impl Fn(libc::uid_t) -> bool,
    ) -> Result<libc::uid_t, RegistryError> {
        if let Some((_, uid)) = self.entries.iter().find(|(eid, _)| eid == id) {
            let end = base.saturating_add(range);
            if *uid < base || *uid >= end {
                eprintln!(
                    "hephaestus-jailer: warning: instance {id:?} keeps its previously \
                     allocated uid {uid}, outside the current --uid-base range [{base}, {end})"
                );
            }
            return Ok(*uid);
        }
        let end = base.saturating_add(range);
        for uid in base..end {
            // uid 0 is excluded unconditionally: `passwd_taken` normally
            // rejects it (root has a passwd entry), but a failed lookup is
            // indistinguishable from a free uid, and "drop" to root must
            // never be one lookup failure away.
            if uid == 0 {
                continue;
            }
            if self.entries.iter().any(|(_, u)| *u == uid) || passwd_taken(uid) {
                continue;
            }
            return Ok(uid);
        }
        Err(RegistryError::Exhausted {
            id: id.to_string(),
            base,
            end,
        })
    }

    /// Record (or refresh) `id`'s uid and persist the registry.
    pub fn upsert(&mut self, id: &str, uid: libc::uid_t) -> Result<(), RegistryError> {
        match self.entries.iter_mut().find(|(eid, _)| eid == id) {
            Some(entry) => entry.1 = uid,
            None => self.entries.push((id.to_string(), uid)),
        }
        self.rewrite()
    }

    /// Drop `id`'s entry (if any) and persist. Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> Result<bool, RegistryError> {
        let before = self.entries.len();
        self.entries.retain(|(eid, _)| eid != id);
        let removed = self.entries.len() != before;
        if removed {
            self.rewrite()?;
        }
        Ok(removed)
    }

    /// Rewrite the whole file in place under the held lock. The file is
    /// tiny; a crash mid-rewrite can only lose lines, which the callers'
    /// re-registration-on-launch behavior heals.
    fn rewrite(&mut self) -> Result<(), RegistryError> {
        let err = |source| RegistryError::Rewrite {
            path: self.path.clone(),
            source,
        };
        let mut out = String::new();
        for (id, uid) in &self.entries {
            out.push_str(id);
            out.push(' ');
            out.push_str(&uid.to_string());
            out.push('\n');
        }
        self.file.rewind().map_err(err)?;
        self.file.set_len(0).map_err(err)?;
        self.file.write_all(out.as_bytes()).map_err(err)?;
        self.file.sync_all().map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("heph-uidreg-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const NONE_TAKEN: fn(libc::uid_t) -> bool = |_| false;

    #[test]
    fn allocate_picks_lowest_free_uid() {
        let dir = scratch("lowest");
        let reg = Registry::open(&dir).unwrap();
        assert_eq!(reg.allocate("a", 61000, 10, NONE_TAKEN).unwrap(), 61000);
    }

    #[test]
    fn allocate_skips_registry_taken_uids() {
        let dir = scratch("skip-taken");
        let mut reg = Registry::open(&dir).unwrap();
        reg.upsert("a", 61000).unwrap();
        reg.upsert("b", 61001).unwrap();
        assert_eq!(reg.allocate("c", 61000, 10, NONE_TAKEN).unwrap(), 61002);
    }

    #[test]
    fn allocate_skips_passwd_uids() {
        let dir = scratch("skip-passwd");
        let reg = Registry::open(&dir).unwrap();
        let taken = |u: libc::uid_t| u < 61002;
        assert_eq!(reg.allocate("a", 61000, 10, taken).unwrap(), 61002);
    }

    #[test]
    fn allocate_reuses_existing_entry_for_id() {
        let dir = scratch("reuse");
        let mut reg = Registry::open(&dir).unwrap();
        reg.upsert("a", 59999).unwrap();
        // Reused even though it lies outside the requested range.
        assert_eq!(reg.allocate("a", 61000, 10, NONE_TAKEN).unwrap(), 59999);
    }

    #[test]
    fn allocate_never_hands_out_uid_zero() {
        let dir = scratch("uid-zero");
        let reg = Registry::open(&dir).unwrap();
        // Even with a passwd probe that fails open (claims every uid is
        // free, root included), uid 0 must be skipped.
        assert_eq!(reg.allocate("a", 0, 10, NONE_TAKEN).unwrap(), 1);
    }

    #[test]
    fn allocate_errors_when_exhausted() {
        let dir = scratch("exhausted");
        let mut reg = Registry::open(&dir).unwrap();
        reg.upsert("a", 61000).unwrap();
        reg.upsert("b", 61001).unwrap();
        assert!(matches!(
            reg.allocate("c", 61000, 2, NONE_TAKEN),
            Err(RegistryError::Exhausted { .. })
        ));
    }

    #[test]
    fn remove_rewrites_without_the_id() {
        let dir = scratch("remove");
        {
            let mut reg = Registry::open(&dir).unwrap();
            reg.upsert("a", 61000).unwrap();
            reg.upsert("b", 61001).unwrap();
            assert!(reg.remove("a").unwrap());
            assert!(!reg.remove("a").unwrap());
        }
        let reg = Registry::open(&dir).unwrap();
        assert_eq!(reg.entries(), &[("b".to_string(), 61001)]);
    }

    #[test]
    fn entries_persist_across_open() {
        let dir = scratch("persist");
        {
            let mut reg = Registry::open(&dir).unwrap();
            reg.upsert("a", 61000).unwrap();
        }
        let reg = Registry::open(&dir).unwrap();
        assert_eq!(reg.allocate("a", 61000, 10, NONE_TAKEN).unwrap(), 61000);
        let mode = fs::metadata(dir.join(REGISTRY_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn corrupt_line_is_rejected() {
        let dir = scratch("corrupt");
        for bad in ["../evil 61000", "a not-a-number", "no-space-line"] {
            fs::write(dir.join(REGISTRY_NAME), format!("{bad}\n")).unwrap();
            assert!(
                matches!(Registry::open(&dir), Err(RegistryError::Corrupt { .. })),
                "line {bad:?} should be rejected"
            );
        }
    }
}

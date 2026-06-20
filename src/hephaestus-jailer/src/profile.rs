//! Generate a deny-by-default macOS sandbox profile for one VM.
//!
//! Port of `scripts/generate-fc-sandbox-profile.sh`. The profile grants
//! the minimum paths and primitives a `hephaestus-firecracker` daemon needs
//! to boot and serve one VM: broad process/sysctl/mach/network access
//! (the profile is file-restrictive, not process-restrictive), read-only
//! access to system paths, explicit read-only access to caller-supplied
//! files (kernel, initramfs), read-write access to caller-supplied writable
//! files (rootfs), and read-write/create/delete under caller-supplied per-VM
//! work directories (api socket, logs, metrics, snapshots, pool slot).
//!
//! The output is the sandbox profile language source the daemon feeds to
//! `sandbox_init(3)`.

use std::path::{Path, PathBuf};

/// Inputs for [`generate`]. All paths are canonicalized before being
/// emitted into the profile, so callers can pass relative paths.
#[derive(Debug, Default)]
pub struct ProfileInputs<'a> {
    /// Per-VM working directories that need read/write/create/delete
    /// access. Typically the api-socket parent, log dir, metrics dir,
    /// snapshot dir, and pool slot rootfs clone.
    pub work_dirs: Vec<&'a Path>,
    /// Per-VM single files that need read/write access. Typically the
    /// api socket itself, log file, metrics file, snapshot blob, mem
    /// stub, and pool machineid sidecar.
    pub read_write_files: Vec<&'a Path>,
    /// Read-only VM inputs. Typically the kernel image, initramfs,
    /// read-only rootfs, and any MMDS JSON file the client pre-staged.
    pub reads: Vec<&'a Path>,
}

/// Generate the sandbox profile source. Caller is responsible for writing
/// it to a file and passing that file's path to `hephaestus-firecracker
/// --sandbox-profile`.
pub fn generate(inputs: &ProfileInputs<'_>) -> Result<String, GenError> {
    let mut out = String::new();
    out.push_str("(version 1)\n(deny default)\n\n");
    out.push_str(
        ";; Basic process/runtime operations. The profile remains file/network\n\
         ;; restrictive; these keep a normal already-exec'd Rust daemon alive.\n\
         (allow process*)\n\
         (allow sysctl-read)\n\
         (allow signal (target self))\n\
         (allow mach-lookup)\n\
         (allow network*)\n\n\
         ;; Allow system metadata reads that libc/Foundation may perform lazily after\n\
         ;; sandbox entry. Data reads remain path-scoped below.\n\
         (allow file-read-metadata)\n\
         (allow file-read-data\n\
          (subpath \"/System\")\n\
          (subpath \"/usr/lib\")\n\
          (subpath \"/private/var/db/timezone\")\n\
          (literal \"/dev/null\")\n\
          (literal \"/dev/urandom\"))\n",
    );

    if !inputs.reads.is_empty() {
        out.push_str("\n;; Explicit read-only VM inputs.\n(allow file-read-data\n");
        for path in &inputs.reads {
            out.push_str(&format!("  {}\n", literal_form(path)?));
        }
        out.push_str(")\n");
    }

    if !inputs.work_dirs.is_empty() || !inputs.read_write_files.is_empty() {
        out.push_str(
            "\n;; Per-VM working directories/files: API socket, logs, metrics, snapshots.\n\
             (allow file-read* file-write*\n",
        );
        for path in &inputs.work_dirs {
            out.push_str(&format!("  {}\n", subpath_form(path)?));
        }
        for path in &inputs.read_write_files {
            out.push_str(&format!("  {}\n", literal_form(path)?));
        }
        out.push_str(")\n");
    }

    Ok(out)
}

/// `(literal "<abs path>")` — for single files. Canonicalizes the path so
/// the profile references the real location even if the caller passed a
/// symlink or relative path.
fn literal_form(path: &Path) -> Result<String, GenError> {
    let canonical = canonicalize(path).map_err(|e| GenError::Canonicalize {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(format!(
        "(literal \"{}\")",
        scheme_escape(&canonical.to_string_lossy())
    ))
}

/// `(subpath "<abs dir>")` — for directories. Creates the dir if missing
/// (sandbox profiles apply before the daemon creates files inside) and
/// canonicalizes so the profile references the real location.
fn subpath_form(path: &Path) -> Result<String, GenError> {
    std::fs::create_dir_all(path).map_err(|e| GenError::CreateDir {
        path: path.to_path_buf(),
        source: e,
    })?;
    let canonical = canonicalize(path).map_err(|e| GenError::Canonicalize {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(format!(
        "(subpath \"{}\")",
        scheme_escape(&canonical.to_string_lossy())
    ))
}

/// Canonicalize that doesn't require the path to exist (for read-only
/// inputs we want a clean error rather than silently emitting a stale
/// path). Falls back to `std::fs::canonicalize` for existing paths.
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Escape `\` and `"` for the Scheme-syntax profile literal.
fn scheme_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Errors from profile generation.
#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("failed to canonicalize {}: {source}", path.display())]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create work dir {}: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_escape_handles_backslashes_and_quotes() {
        assert_eq!(scheme_escape(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(scheme_escape("plain"), "plain");
    }

    #[test]
    fn generate_with_empty_inputs_is_deny_default() {
        let out = generate(&ProfileInputs::default()).unwrap();
        assert!(out.starts_with("(version 1)\n(deny default)"));
        assert!(!out.contains("(allow file-read-data\n  (literal"));
        assert!(!out.contains("(allow file-read* file-write*"));
    }

    #[test]
    fn generate_includes_read_paths() {
        let tmp = std::env::temp_dir().join("hephaestus-jailer-test-read");
        std::fs::write(&tmp, b"x").unwrap();
        let inputs = ProfileInputs {
            reads: vec![&tmp],
            ..Default::default()
        };
        let out = generate(&inputs).unwrap();
        let canonical = std::fs::canonicalize(&tmp).unwrap();
        assert!(out.contains(";; Explicit read-only VM inputs."));
        assert!(out.contains(&format!("(literal \"{}\")", canonical.to_string_lossy())));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn generate_creates_work_dir_and_grants_subpath() {
        let tmp = std::env::temp_dir().join("hephaestus-jailer-test-workdir");
        let _ = std::fs::remove_dir_all(&tmp);
        let inputs = ProfileInputs {
            work_dirs: vec![&tmp],
            ..Default::default()
        };
        let out = generate(&inputs).unwrap();
        assert!(tmp.is_dir(), "work_dir should be created");
        assert!(out.contains("(subpath \""));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

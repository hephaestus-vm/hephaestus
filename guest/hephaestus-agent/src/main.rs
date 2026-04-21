//! hephaestus-agent
//!
//! The tiniest useful Linux init: the kernel hands us PID 1 from an
//! initramfs, we mount the container rootfs off `/dev/vda`, `chroot` into
//! it, exec the command the host encoded in `/proc/cmdline`, print an
//! exit-code sentinel back through the serial console, and halt.
//!
//! Host-facing contract:
//!
//! - Input via kernel cmdline key `hephaestus.cmd=<hex>` where `<hex>` is
//!   the command string hex-encoded (avoids quoting / space issues in the
//!   cmdline parser — hex characters are safe everywhere).
//! - Output via the standard serial console. Host watches for
//!   `__HEPHAESTUS_EXIT_<code>__\n` to parse the exit code.
//! - The guest shuts down via `reboot(RB_POWER_OFF)` when done, so the
//!   host's KVO observer sees a clean `.stopped` state transition.

use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use nix::mount::{MsFlags, mount};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, chroot, execv, fork};

type AgentResult<T> = Result<T, String>;

fn main() -> ExitCode {
    // Print a banner early so the host serial log proves we're alive even
    // if mount or exec fails before the exit sentinel.
    eprintln!("hephaestus-agent: init starting (pid {})", std::process::id());

    match run() {
        Ok(()) => {
            // run() always halts on success; unreachable but keeps the
            // signature tidy.
            eprintln!("hephaestus-agent: run returned Ok unexpectedly");
            halt();
        }
        Err(e) => {
            // Failure path: emit the sentinel so the host doesn't hang
            // waiting for it, then halt.
            eprintln!("hephaestus-agent: fatal: {e}");
            println!("\n__HEPHAESTUS_EXIT_127__");
            halt();
        }
    }
}

fn run() -> AgentResult<()> {
    mount_essentials()?;

    let cmd = read_command_from_cmdline()?;
    eprintln!("hephaestus-agent: cmd = {cmd:?}");

    mount_rootfs("/dev/vda", "/newroot")?;
    enter_rootfs("/newroot")?;

    let exit_code = run_command(&cmd)?;

    // Must be the last printed line — host consumes everything up to here.
    println!("\n__HEPHAESTUS_EXIT_{exit_code}__");
    halt();
}

// =============================================================================
// Mount helpers.
// =============================================================================

fn mount_essentials() -> AgentResult<()> {
    // /proc is required to read cmdline; others help guest programs behave
    // normally. devtmpfs populates /dev/vda, without which we can't mount
    // the rootfs.
    mkdir_p("/proc")?;
    mkdir_p("/sys")?;
    mkdir_p("/dev")?;
    mkdir_p("/tmp")?;

    do_mount(Some("proc"), "/proc", Some("proc"), MsFlags::empty())?;
    // sysfs / devtmpfs are nice-to-have but not fatal if they fail on some
    // exotic kernel build.
    let _ = do_mount(Some("sys"), "/sys", Some("sysfs"), MsFlags::empty());
    let _ = do_mount(Some("dev"), "/dev", Some("devtmpfs"), MsFlags::empty());
    let _ = do_mount(Some("tmpfs"), "/tmp", Some("tmpfs"), MsFlags::empty());
    Ok(())
}

fn mount_rootfs(source: &str, target: &str) -> AgentResult<()> {
    mkdir_p(target)?;
    // Try ext4 first; most container rootfses we'll be pointed at are ext4.
    do_mount(Some(source), target, Some("ext4"), MsFlags::empty())
        .map_err(|e| format!("mounting rootfs {source} → {target} as ext4: {e}"))
}

fn enter_rootfs(new_root: &str) -> AgentResult<()> {
    // Bind essential vfs mounts into the new root so programs that read
    // /proc (coreutils, busybox, python, …) keep working after chroot.
    mkdir_p(&format!("{new_root}/proc"))?;
    mkdir_p(&format!("{new_root}/sys"))?;
    mkdir_p(&format!("{new_root}/dev"))?;
    mkdir_p(&format!("{new_root}/tmp"))?;

    let _ = do_mount(Some("/proc"), &format!("{new_root}/proc"), None, MsFlags::MS_BIND);
    let _ = do_mount(Some("/sys"), &format!("{new_root}/sys"), None, MsFlags::MS_BIND);
    let _ = do_mount(Some("/dev"), &format!("{new_root}/dev"), None, MsFlags::MS_BIND);
    let _ = do_mount(Some("/tmp"), &format!("{new_root}/tmp"), None, MsFlags::MS_BIND);

    chroot(new_root).map_err(|e| format!("chroot({new_root}): {e}"))?;
    chdir("/").map_err(|e| format!("chdir(/): {e}"))?;
    Ok(())
}

fn do_mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: MsFlags,
) -> AgentResult<()> {
    mount::<str, str, str, str>(source, target, fstype, flags, None)
        .map_err(|e| format!("mount({target}, fs={fstype:?}): {e}"))
}

fn mkdir_p(path: &str) -> AgentResult<()> {
    if Path::new(path).exists() {
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|e| format!("mkdir -p {path}: {e}"))
}

// =============================================================================
// Command parsing.
// =============================================================================

fn read_command_from_cmdline() -> AgentResult<String> {
    let raw = fs::read_to_string("/proc/cmdline")
        .map_err(|e| format!("reading /proc/cmdline: {e}"))?;
    let hex = raw
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("hephaestus.cmd="))
        .ok_or_else(|| String::from("missing hephaestus.cmd= on kernel cmdline"))?;
    let bytes = hex_decode(hex)?;
    String::from_utf8(bytes).map_err(|e| format!("command is not UTF-8: {e}"))
}

fn hex_decode(s: &str) -> AgentResult<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return Err(format!("hex length {} is odd", b.len()));
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for chunk in b.chunks(2) {
        let hi = nibble(chunk[0])?;
        let lo = nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> AgentResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("bad hex char {:?}", c as char)),
    }
}

// =============================================================================
// Command execution.
// =============================================================================

fn run_command(cmd: &str) -> AgentResult<i32> {
    let sh = CString::new("/bin/sh").unwrap();
    let flag = CString::new("-c").unwrap();
    let cmd_c = CString::new(cmd).map_err(|e| format!("cmd has NUL byte: {e}"))?;

    // SAFETY: fork() is unsafe because a multi-threaded parent can leave
    // the child in a bad state. We're single-threaded here by construction.
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Parent { child } => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => Ok(code),
            Ok(WaitStatus::Signaled(_, sig, _)) => Ok(128 + sig as i32),
            Ok(other) => Err(format!("unexpected wait status: {other:?}")),
            Err(e) => Err(format!("waitpid: {e}")),
        },
        ForkResult::Child => {
            let _ = execv(&sh, &[sh.as_c_str(), flag.as_c_str(), cmd_c.as_c_str()]);
            // Only reached if execv failed.
            eprintln!("hephaestus-agent: execv failed");
            std::process::exit(127);
        }
    }
}

// =============================================================================
// Shutdown.
// =============================================================================

fn halt() -> ! {
    // Give the kernel a moment to drain the serial buffer so the exit
    // sentinel actually reaches the host before we power off.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // sync()
    unsafe { libc::sync() };
    match reboot(RebootMode::RB_POWER_OFF) {
        Ok(_) => {}
        Err(e) => eprintln!("hephaestus-agent: reboot(POWER_OFF) failed: {e}"),
    }
    // If reboot() somehow returns, park forever.
    loop {
        std::thread::park();
    }
}

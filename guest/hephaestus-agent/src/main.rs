//! hephaestus-agent
//!
//! Minimal Linux init for the hephaestus direct-VZ path. Host-facing
//! contract:
//!
//! 1. The kernel hands us PID 1 out of the initramfs.
//! 2. We mount /proc, /sys, /dev, the container rootfs on /dev/vda, then
//!    `chroot` into the rootfs.
//! 3. We listen on vsock port 1234. The host connects and sends a
//!    length-prefixed UTF-8 command string.
//! 4. We `fork` + `exec /bin/sh -c <cmd>` and `waitpid`.
//! 5. We write the exit code back over the same vsock connection as a
//!    little-endian i32, close, `sync`, and call
//!    `reboot(RB_POWER_OFF)` so the host observes a clean `.stopped`
//!    state transition.
//!
//! The wire protocol is intentionally trivial — no framing beyond the
//! length prefix — because there's exactly one exchange per boot.

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
// OwnedFd is used via its AsRawFd impl on the listen socket.
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

use nix::mount::{MsFlags, mount};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept, bind, connect, listen, socket,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, chroot, execv, fork};

type AgentResult<T> = Result<T, String>;

/// Vsock port the agent listens on. Hard-coded because there's only one
/// agent per VM; the host knows where to connect.
const COMMAND_PORT: u32 = 1234;
const MMDS_VSOCK_PORT: u32 = 16_992;
const VMADDR_CID_HOST: u32 = 2;

fn main() -> ExitCode {
    eprintln!("hephaestus-agent: init starting (pid {})", std::process::id());
    match run() {
        Ok(()) => {
            eprintln!("hephaestus-agent: run returned Ok unexpectedly");
            halt();
        }
        Err(e) => {
            eprintln!("hephaestus-agent: fatal: {e}");
            halt();
        }
    }
}

fn run() -> AgentResult<()> {
    mount_essentials()?;
    mount_rootfs("/dev/vda", "/newroot")?;
    enter_rootfs("/newroot")?;

    // Open the vsock socket BEFORE the host tries to connect. Once listen
    // has returned, any connect from the host will succeed.
    let listen_fd = vsock_listen(COMMAND_PORT)?;
    eprintln!("hephaestus-agent: listening on vsock port {COMMAND_PORT}");

    // Loop accepting connections until we receive a real command. This
    // lets the host "probe" by connecting without writing (to confirm the
    // agent is live before snapshotting) without using up our single
    // command slot.
    let (command, mut stream) = accept_command_loop(&listen_fd)?;
    eprintln!("hephaestus-agent: cmd = {command:?}");

    let exit_code = run_command(&command)?;
    write_exit_code(&mut stream, exit_code)?;
    drop(stream); // flush before halt
    halt();
}

fn accept_command_loop(listen_fd: &OwnedFd) -> AgentResult<(String, UnixStream)> {
    loop {
        let conn_raw = accept(listen_fd.as_raw_fd()).map_err(|e| format!("vsock accept: {e}"))?;
        // SAFETY: `accept` returns a freshly-allocated fd we own exclusively.
        let mut stream = unsafe { UnixStream::from_raw_fd(conn_raw) };
        match read_command(&mut stream) {
            Ok(cmd) if !cmd.is_empty() => return Ok((cmd, stream)),
            _ => {
                // Probe connection (host closed without writing, or sent a
                // zero-length frame). Drop the stream and wait for the
                // real command to arrive.
                drop(stream);
            }
        }
    }
}

// =============================================================================
// Mount helpers.
// =============================================================================

fn mount_essentials() -> AgentResult<()> {
    mkdir_p("/proc")?;
    mkdir_p("/sys")?;
    mkdir_p("/dev")?;
    mkdir_p("/tmp")?;
    do_mount(Some("proc"), "/proc", Some("proc"), MsFlags::empty())?;
    let _ = do_mount(Some("sys"), "/sys", Some("sysfs"), MsFlags::empty());
    let _ = do_mount(Some("dev"), "/dev", Some("devtmpfs"), MsFlags::empty());
    let _ = do_mount(Some("tmpfs"), "/tmp", Some("tmpfs"), MsFlags::empty());
    Ok(())
}

fn mount_rootfs(source: &str, target: &str) -> AgentResult<()> {
    mkdir_p(target)?;
    do_mount(Some(source), target, Some("ext4"), MsFlags::empty())
        .map_err(|e| format!("mounting rootfs {source} → {target} as ext4: {e}"))
}

fn enter_rootfs(new_root: &str) -> AgentResult<()> {
    for sub in ["proc", "sys", "dev", "tmp"] {
        mkdir_p(&format!("{new_root}/{sub}"))?;
    }
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
// Vsock listener + framed I/O.
// =============================================================================

fn vsock_listen(port: u32) -> AgentResult<OwnedFd> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| format!("vsock socket: {e}"))?;
    // CID_ANY (0xFFFFFFFF) binds to whichever CID the kernel assigns us.
    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, port);
    bind(fd.as_raw_fd(), &addr).map_err(|e| format!("vsock bind port {port}: {e}"))?;
    listen(&fd, Backlog::new(1).unwrap()).map_err(|e| format!("vsock listen: {e}"))?;
    Ok(fd)
}

fn read_command(stream: &mut UnixStream) -> AgentResult<String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("reading command length: {e}"))?;
    let len = u32::from_le_bytes(len_buf);
    if len > 1 << 20 {
        return Err(format!("command length {len} exceeds 1 MiB sanity cap"));
    }
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .map_err(|e| format!("reading command body: {e}"))?;
    String::from_utf8(buf).map_err(|e| format!("command is not UTF-8: {e}"))
}

fn write_exit_code(stream: &mut UnixStream, code: i32) -> AgentResult<()> {
    let bytes = code.to_le_bytes();
    stream
        .write_all(&bytes)
        .map_err(|e| format!("writing exit code: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flushing exit code: {e}"))?;
    Ok(())
}

// =============================================================================
// Command execution.
// =============================================================================

fn run_command(cmd: &str) -> AgentResult<i32> {
    if let Some(needle) = cmd.strip_prefix("__hephaestus_test_mmds_vsock ") {
        return test_mmds_vsock(needle).map(|()| 0).or_else(|err| {
            eprintln!("hephaestus-agent: mmds-vsock test failed: {err}");
            Ok(1)
        });
    }
    if let Some(args) = cmd.strip_prefix("__hephaestus_test_vsock_suite ") {
        return test_vsock_suite(args).map(|()| 0).or_else(|err| {
            eprintln!("hephaestus-agent: vsock suite failed: {err}");
            Ok(1)
        });
    }

    let sh = CString::new("/bin/sh").unwrap();
    let flag = CString::new("-c").unwrap();
    let cmd_c = CString::new(cmd).map_err(|e| format!("cmd has NUL byte: {e}"))?;

    // SAFETY: single-threaded agent; post-fork we only call async-signal-safe
    // operations before execv.
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Parent { child } => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => Ok(code),
            Ok(WaitStatus::Signaled(_, sig, _)) => Ok(128 + sig as i32),
            Ok(other) => Err(format!("unexpected wait status: {other:?}")),
            Err(e) => Err(format!("waitpid: {e}")),
        },
        ForkResult::Child => {
            let _ = execv(&sh, &[sh.as_c_str(), flag.as_c_str(), cmd_c.as_c_str()]);
            // execv only returns on error.
            eprintln!("hephaestus-agent: execv failed");
            std::process::exit(127);
        }
    }
}

fn test_mmds_vsock(needle: &str) -> AgentResult<()> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| format!("mmds vsock socket: {e}"))?;
    let addr = VsockAddr::new(VMADDR_CID_HOST, MMDS_VSOCK_PORT);
    connect(fd.as_raw_fd(), &addr).map_err(|e| format!("mmds vsock connect: {e}"))?;
    // SAFETY: socket returned a fresh fd and connect transferred no ownership.
    let mut stream = unsafe { UnixStream::from_raw_fd(fd.as_raw_fd()) };
    // Prevent OwnedFd from closing the descriptor now owned by UnixStream.
    std::mem::forget(fd);
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: mmds\r\nConnection: close\r\n\r\n")
        .map_err(|e| format!("mmds request write: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("mmds response read: {e}"))?;
    if !response.contains(needle) {
        return Err(format!(
            "mmds response did not contain {needle:?}: {response:?}"
        ));
    }
    eprintln!("hephaestus-agent: mmds-vsock test matched {needle:?}");
    Ok(())
}

fn test_vsock_suite(args: &str) -> AgentResult<()> {
    let mut parts = args.splitn(3, ' ');
    let needle = parts
        .next()
        .ok_or_else(|| "missing MMDS response needle".to_string())?;
    let echo_port = parts
        .next()
        .ok_or_else(|| "missing echo port".to_string())?
        .parse::<u32>()
        .map_err(|e| format!("invalid echo port: {e}"))?;
    let token = parts
        .next()
        .ok_or_else(|| "missing echo token".to_string())?
        .as_bytes()
        .to_vec();

    test_mmds_vsock(needle)?;
    test_vsock_echo(echo_port, &token)?;
    Ok(())
}

fn test_vsock_echo(port: u32, expected: &[u8]) -> AgentResult<()> {
    let listen_fd = vsock_listen(port)?;
    eprintln!("hephaestus-agent: generic echo listening on vsock port {port}");
    let conn_raw = accept(listen_fd.as_raw_fd()).map_err(|e| format!("echo accept: {e}"))?;
    // SAFETY: `accept` returns a freshly-allocated fd we own exclusively.
    let mut stream = unsafe { UnixStream::from_raw_fd(conn_raw) };
    let mut payload = vec![0u8; expected.len()];
    stream
        .read_exact(&mut payload)
        .map_err(|e| format!("echo payload read: {e}"))?;
    if payload != expected {
        return Err(format!(
            "echo payload mismatch: got {payload:?}, expected {expected:?}"
        ));
    }
    stream
        .write_all(&payload)
        .map_err(|e| format!("echo payload write: {e}"))?;
    stream.flush().map_err(|e| format!("echo flush: {e}"))?;
    eprintln!(
        "hephaestus-agent: generic echo matched {} bytes on vsock port {port}",
        payload.len()
    );
    Ok(())
}

// =============================================================================
// Shutdown.
// =============================================================================

fn halt() -> ! {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { libc::sync() };
    match reboot(RebootMode::RB_POWER_OFF) {
        Ok(_) => {}
        Err(e) => eprintln!("hephaestus-agent: reboot(POWER_OFF) failed: {e}"),
    }
    loop {
        std::thread::park();
    }
}

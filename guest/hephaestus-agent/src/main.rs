//! hephaestus-agent
//!
//! Minimal Linux init for the hephaestus direct-VZ path. Host-facing
//! contract:
//!
//! 1. The kernel hands us PID 1 out of the initramfs.
//! 2. We mount /proc, /sys, /dev, the container rootfs on /dev/vda, then
//!    `chroot` into the rootfs.
//! 3. Unless disabled with `HEPHAESTUS_MMDS_SHIM=0` or
//!    `hephaestus.mmds=off`, we start a guest-side link-local MMDS shim:
//!    `http://169.254.169.254/` forwards to host vsock port 16992.
//! 4. We listen on vsock port 1234. The host connects and sends a
//!    length-prefixed UTF-8 command string.
//! 5. We `fork` + `exec /bin/sh -c <cmd>` and `waitpid`.
//! 6. We write the exit code back over the same vsock connection as a
//!    little-endian i32, close, `sync`, and call
//!    `reboot(RB_POWER_OFF)` so the host observes a clean `.stopped`
//!    state transition.
//!
//! `hephaestus-agent mmds-shim` also runs the same shim as a foreground
//! helper for custom images that want to launch it from their own init.
//!
//! The command wire protocol is intentionally trivial — no framing beyond
//! the length prefix — because there's exactly one exchange per boot.

use std::env;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
// OwnedFd is used via its AsRawFd impl on the listen socket.
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use nix::mount::{MsFlags, mount};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept, bind, connect, listen, socket,
};
use nix::unistd::{chdir, chroot};

type AgentResult<T> = Result<T, String>;

/// Vsock port the agent listens on. Hard-coded because there's only one
/// agent per VM; the host knows where to connect.
const COMMAND_PORT: u32 = 1234;
const MMDS_VSOCK_PORT: u32 = 16_992;
const MMDS_LINKLOCAL_ADDR: &str = "169.254.169.254";
const MMDS_LINKLOCAL_PORT: u16 = 80;
const VMADDR_CID_HOST: u32 = 2;

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some("mmds-shim") {
        return match run_mmds_linklocal_shim() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("hephaestus-agent: mmds-shim fatal: {e}");
                ExitCode::FAILURE
            }
        };
    }

    eprintln!(
        "hephaestus-agent: init starting (pid {})",
        std::process::id()
    );
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

    // Redirect stderr to the second virtio-console device (hvc1) so the
    // host sees stdout and stderr on separate streams. hvc0 keeps agent
    // diagnostics + the child's stdout; hvc1 carries only the child's
    // stderr. Best-effort: if /dev/hvc1 isn't present (older kernel or
    // VZ config without the second serial), stderr stays on hvc0 and
    // the two streams stay merged — same as before this change.
    let _ = redirect_stderr_to_hvc1();

    maybe_start_mmds_linklocal_shim();

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

    let exit_code = run_command(&command, &mut stream)?;
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
    let _ = do_mount(
        Some("/proc"),
        &format!("{new_root}/proc"),
        None,
        MsFlags::MS_BIND,
    );
    let _ = do_mount(
        Some("/sys"),
        &format!("{new_root}/sys"),
        None,
        MsFlags::MS_BIND,
    );
    let _ = do_mount(
        Some("/dev"),
        &format!("{new_root}/dev"),
        None,
        MsFlags::MS_BIND,
    );
    let _ = do_mount(
        Some("/tmp"),
        &format!("{new_root}/tmp"),
        None,
        MsFlags::MS_BIND,
    );
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

/// Redirect fd 2 (stderr) to `/dev/hvc1`, the second virtio-console
/// device. The host's `ExecSession.make` attaches a second serial port
/// whose read end is forwarded to the host's stderr; dups ensure the
/// child `/bin/sh -c CMD` inherits that fd as its own stderr, so the
/// two streams stay separated on the host. Best-effort: a missing
/// `/dev/hvc1` (older config, snapshot-restore path that uses URL
/// serial attachments only) leaves stderr on hvc0 and the streams
/// remain merged — same as the pre-stderr-split behavior.
fn redirect_stderr_to_hvc1() -> AgentResult<()> {
    use std::os::fd::AsRawFd;
    let hvc1 = match fs::OpenOptions::new().write(true).open("/dev/hvc1") {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            eprintln!("hephaestus-agent: /dev/hvc1 not present; stderr stays merged with stdout");
            return Ok(());
        }
        Err(e) => return Err(format!("open /dev/hvc1: {e}")),
    };
    if unsafe { libc::dup2(hvc1.as_raw_fd(), 2) } < 0 {
        return Err(format!(
            "dup2(/dev/hvc1, 2): {}",
            std::io::Error::last_os_error()
        ));
    }
    eprintln!("hephaestus-agent: stderr redirected to /dev/hvc1");
    Ok(())
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

fn run_command(cmd: &str, stream: &mut UnixStream) -> AgentResult<i32> {
    if let Some(needle) = cmd.strip_prefix("__hephaestus_test_mmds_vsock ") {
        return test_mmds_vsock(needle).map(|()| 0).or_else(|err| {
            eprintln!("hephaestus-agent: mmds-vsock test failed: {err}");
            Ok(1)
        });
    }
    if let Some(needle) = cmd.strip_prefix("__hephaestus_test_mmds_linklocal ") {
        return test_mmds_linklocal(needle).map(|()| 0).or_else(|err| {
            eprintln!("hephaestus-agent: mmds-linklocal test failed: {err}");
            Ok(1)
        });
    }
    if let Some(args) = cmd.strip_prefix("__hephaestus_test_vsock_suite ") {
        return test_vsock_suite(args).map(|()| 0).or_else(|err| {
            eprintln!("hephaestus-agent: vsock suite failed: {err}");
            Ok(1)
        });
    }

    let (cmd, forward_stdin) = match cmd.strip_prefix("__hephaestus_stdin__") {
        Some(rest) => (rest, true),
        None => (cmd, false),
    };

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(if forward_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .spawn()
        .map_err(|e| format!("exec /bin/sh: {e}"))?;

    if forward_stdin {
        if let Some(mut child_stdin) = child.stdin.take() {
            let mut stream_clone = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("hephaestus-agent: stdin stream clone failed: {e}");
                    let _ = child.kill();
                    return Err(format!("clone stdin stream: {e}"));
                }
            };
            let _stdin_pump = std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stream_clone.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if child_stdin.write_all(&buf[..n]).is_err() {
                                break;
                            }
                            if child_stdin.flush().is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            let status = child.wait().map_err(|e| format!("wait /bin/sh: {e}"))?;
            // Do not join the stdin pump here. If the child exits before
            // consuming all host stdin, the host may still be blocked reading
            // from its stdin and may not half-close the vsock write side yet.
            // Writing the exit code is safe on the other half of the socket;
            // the agent process halts immediately afterwards.
            return Ok(match status.code() {
                Some(code) => code,
                None => 128 + status.signal().unwrap_or(0),
            });
        }
    }

    let status = child.wait().map_err(|e| format!("wait /bin/sh: {e}"))?;
    Ok(match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    })
}

fn fetch_mmds_vsock_response() -> AgentResult<Vec<u8>> {
    fetch_mmds_vsock_response_for_request(
        b"GET / HTTP/1.1\r\nHost: mmds\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    )
}

fn fetch_mmds_vsock_response_for_request(request: &[u8]) -> AgentResult<Vec<u8>> {
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
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("mmds vsock set read timeout: {e}"))?;
    stream
        .write_all(request)
        .map_err(|e| format!("mmds request write: {e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("mmds response read: {e}"))?;
    Ok(response)
}

fn test_mmds_vsock(needle: &str) -> AgentResult<()> {
    let response = fetch_mmds_vsock_response()?;
    let response = String::from_utf8_lossy(&response);
    if !response.contains(needle) {
        return Err(format!(
            "mmds response did not contain {needle:?}: {response:?}"
        ));
    }
    eprintln!("hephaestus-agent: mmds-vsock test matched {needle:?}");
    Ok(())
}

fn configure_linklocal_loopback() -> AgentResult<()> {
    // Prefer `ip` when present, but fall back to busybox ifconfig. The agent
    // runs as root/PID1, so this is enough for our controlled e2e rootfs while
    // the longer-term transparent host-network MMDS path remains separate.
    let script = format!(
        r#"
        (ip link set lo up 2>/dev/null || ifconfig lo up 2>/dev/null || true)
        (ip addr add {addr}/32 dev lo 2>/dev/null || \
         ifconfig lo:heph {addr} netmask 255.255.255.255 up 2>/dev/null || true)
    "#,
        addr = MMDS_LINKLOCAL_ADDR
    );
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .status()
        .map_err(|e| format!("configure link-local loopback: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("configure link-local loopback exited {status}"))
    }
}

fn mmds_linklocal_enabled() -> bool {
    if matches!(
        env::var("HEPHAESTUS_MMDS_SHIM").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    ) {
        return false;
    }
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    !cmdline.split_ascii_whitespace().any(|arg| {
        matches!(
            arg,
            "hephaestus.mmds=0"
                | "hephaestus.mmds=off"
                | "hephaestus.mmds=false"
                | "hephaestus.mmds_shim=0"
                | "hephaestus.mmds_shim=off"
                | "hephaestus.mmds_shim=false"
        )
    })
}

fn maybe_start_mmds_linklocal_shim() {
    if !mmds_linklocal_enabled() {
        eprintln!("hephaestus-agent: link-local MMDS shim disabled");
        return;
    }
    std::thread::spawn(|| {
        if let Err(err) = run_mmds_linklocal_shim() {
            eprintln!("hephaestus-agent: link-local MMDS shim stopped: {err}");
        }
    });
}

fn run_mmds_linklocal_shim() -> AgentResult<()> {
    configure_linklocal_loopback()?;
    let listener = TcpListener::bind((MMDS_LINKLOCAL_ADDR, MMDS_LINKLOCAL_PORT))
        .map_err(|e| format!("bind {MMDS_LINKLOCAL_ADDR}:{MMDS_LINKLOCAL_PORT}: {e}"))?;
    eprintln!(
        "hephaestus-agent: link-local MMDS shim listening on http://{MMDS_LINKLOCAL_ADDR}:{MMDS_LINKLOCAL_PORT}/ -> vsock:{MMDS_VSOCK_PORT}"
    );
    for conn in listener.incoming() {
        match conn {
            Ok(client) => {
                std::thread::spawn(move || {
                    if let Err(err) = handle_mmds_linklocal_client(client) {
                        eprintln!("hephaestus-agent: link-local MMDS request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("hephaestus-agent: link-local MMDS accept failed: {err}"),
        }
    }
    Ok(())
}

fn handle_mmds_linklocal_client(mut client: TcpStream) -> AgentResult<()> {
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("mmds tcp set timeout: {e}"))?;
    let request = read_http_request_head(&mut client)?;
    let response = fetch_mmds_vsock_response_for_request(&request)?;
    client
        .write_all(&response)
        .map_err(|e| format!("mmds tcp response write: {e}"))?;
    client.flush().map_err(|e| format!("mmds tcp flush: {e}"))?;
    Ok(())
}

fn read_http_request_head(client: &mut TcpStream) -> AgentResult<Vec<u8>> {
    let mut request = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") || request.len() >= 16 * 1024 {
                    break;
                }
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
            Err(err) => return Err(format!("mmds tcp request read: {err}")),
        }
    }
    if request.is_empty() {
        request.extend_from_slice(
            b"GET / HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n\r\n",
        );
    }
    Ok(request)
}

fn fetch_mmds_linklocal_response() -> AgentResult<String> {
    let mut stream = TcpStream::connect((MMDS_LINKLOCAL_ADDR, MMDS_LINKLOCAL_PORT))
        .map_err(|e| format!("connect {MMDS_LINKLOCAL_ADDR}:{MMDS_LINKLOCAL_PORT}: {e}"))?;
    stream
        .write_all(
            b"GET / HTTP/1.1\r\nHost: 169.254.169.254\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        )
        .map_err(|e| format!("link-local request write: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("link-local response read: {e}"))?;
    Ok(response)
}

fn test_mmds_linklocal(needle: &str) -> AgentResult<()> {
    let response = match fetch_mmds_linklocal_response() {
        Ok(response) => response,
        Err(first_err) => {
            std::thread::spawn(|| {
                if let Err(err) = run_mmds_linklocal_shim() {
                    eprintln!("hephaestus-agent: test link-local MMDS shim stopped: {err}");
                }
            });
            std::thread::sleep(Duration::from_millis(50));
            fetch_mmds_linklocal_response().map_err(|retry_err| {
                format!("{retry_err}; initial attempt failed with: {first_err}")
            })?
        }
    };
    if !response.contains(needle) {
        return Err(format!(
            "link-local MMDS response did not contain {needle:?}: {response:?}"
        ));
    }
    eprintln!("hephaestus-agent: mmds-linklocal test matched {needle:?}");
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
    test_mmds_linklocal(needle)?;
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

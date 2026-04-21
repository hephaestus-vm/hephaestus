use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use hephaestus_vmm::{
    Compression, Spec, StdioSink, build_rootfs_from_tar, vz_boot, vz_sh, vz_snapshot_restore,
    vz_snapshot_save,
};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("ping") => {
            println!("{}", hephaestus_vmm::ping());
            ExitCode::SUCCESS
        }
        Some("run") => match parse_run_args(&mut args) {
            Ok(opts) => run(opts),
            Err(msg) => {
                eprintln!("hephaestus: {msg}");
                eprintln!("{RUN_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("rootfs") => match parse_rootfs_args(&mut args) {
            Ok(opts) => rootfs(opts),
            Err(msg) => {
                eprintln!("hephaestus: {msg}");
                eprintln!("{ROOTFS_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("vz-boot") => match parse_vz_boot_args(&mut args) {
            Ok(opts) => vz_boot_cmd(opts),
            Err(msg) => {
                eprintln!("hephaestus: {msg}");
                eprintln!("{VZ_BOOT_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("vz-sh") => match parse_vz_sh_args(&mut args) {
            Ok(opts) => vz_sh_cmd(opts),
            Err(msg) => {
                eprintln!("hephaestus: {msg}");
                eprintln!("{VZ_SH_USAGE}");
                ExitCode::from(2)
            }
        },
        Some("vz-snapshot") => match args.next().as_deref() {
            Some("save") => match parse_vz_snap_args(&mut args, true) {
                Ok(opts) => vz_snap_save_cmd(opts),
                Err(msg) => {
                    eprintln!("hephaestus: {msg}");
                    eprintln!("{VZ_SNAP_USAGE}");
                    ExitCode::from(2)
                }
            },
            Some("restore") => match parse_vz_snap_args(&mut args, false) {
                Ok(opts) => vz_snap_restore_cmd(opts),
                Err(msg) => {
                    eprintln!("hephaestus: {msg}");
                    eprintln!("{VZ_SNAP_USAGE}");
                    ExitCode::from(2)
                }
            },
            _ => {
                eprintln!("{VZ_SNAP_USAGE}");
                ExitCode::from(2)
            }
        },
        Some(other) => {
            eprintln!("hephaestus: unknown subcommand `{other}`");
            eprintln!("usage: hephaestus <ping|run|rootfs|vz-boot|vz-sh|vz-snapshot>");
            ExitCode::from(2)
        }
        None => {
            eprintln!("usage: hephaestus <ping|run|rootfs|vz-boot|vz-sh|vz-snapshot>");
            ExitCode::from(2)
        }
    }
}

const RUN_USAGE: &str = "\
usage: hephaestus run \\
    --id <id> \\
    --kernel <path> \\
    --initfs <path-to-ext4> \\
    --rootfs <path-to-ext4> \\
    [--cpus N] [--memory-mib N] [--cwd <path>] \\
    [--network [--ip OCTET]] [--tty] \\
    -- <argv...>";

#[derive(Debug)]
struct RunOptions {
    id: String,
    kernel: PathBuf,
    initfs: PathBuf,
    rootfs: PathBuf,
    cpus: Option<u32>,
    memory_mib: Option<u64>,
    cwd: Option<String>,
    argv: Vec<String>,
    network: bool,
    /// Last octet in VZ's 192.168.64.0/24 NAT. None → derive from id.
    ip_octet: Option<u8>,
    tty: bool,
}

fn parse_run_args(args: &mut impl Iterator<Item = String>) -> Result<RunOptions, String> {
    let mut opts = RunOptions {
        id: "hephaestus-vm".into(),
        kernel: PathBuf::new(),
        initfs: PathBuf::new(),
        rootfs: PathBuf::new(),
        cpus: None,
        memory_mib: None,
        cwd: None,
        argv: Vec::new(),
        network: false,
        ip_octet: None,
        tty: false,
    };
    let mut past_double_dash = false;
    while let Some(arg) = args.next() {
        if past_double_dash {
            opts.argv.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => past_double_dash = true,
            "--id" => opts.id = require_value(args, "--id")?,
            "--kernel" => opts.kernel = require_value(args, "--kernel")?.into(),
            "--initfs" => opts.initfs = require_value(args, "--initfs")?.into(),
            "--rootfs" => opts.rootfs = require_value(args, "--rootfs")?.into(),
            "--cpus" => {
                opts.cpus = Some(
                    require_value(args, "--cpus")?
                        .parse()
                        .map_err(|e| format!("invalid --cpus: {e}"))?,
                )
            }
            "--memory-mib" => {
                opts.memory_mib = Some(
                    require_value(args, "--memory-mib")?
                        .parse()
                        .map_err(|e| format!("invalid --memory-mib: {e}"))?,
                )
            }
            "--cwd" => opts.cwd = Some(require_value(args, "--cwd")?),
            "--network" => opts.network = true,
            "--ip" => {
                let raw = require_value(args, "--ip")?;
                // Accept either `N` or `192.168.64.N`; we only care about
                // the last octet since VZ's NAT subnet is fixed.
                let last = raw.rsplit('.').next().unwrap_or(raw.as_str());
                let n: u8 = last.parse().map_err(|e| format!("invalid --ip: {e}"))?;
                if !(2..=254).contains(&n) {
                    return Err(format!("--ip {n} out of range [2, 254]"));
                }
                opts.ip_octet = Some(n);
            }
            "--tty" => opts.tty = true,
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if opts.kernel.as_os_str().is_empty() {
        return Err("missing --kernel".into());
    }
    if opts.initfs.as_os_str().is_empty() {
        return Err("missing --initfs".into());
    }
    if opts.rootfs.as_os_str().is_empty() {
        return Err("missing --rootfs".into());
    }
    for (label, path) in [
        ("--kernel", &opts.kernel),
        ("--initfs", &opts.initfs),
        ("--rootfs", &opts.rootfs),
    ] {
        if !path.exists() {
            return Err(format!("{label} path does not exist: {}", path.display()));
        }
    }
    Ok(opts)
}

fn require_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    args.next().ok_or_else(|| format!("missing value for {flag}"))
}

fn run(opts: RunOptions) -> ExitCode {
    let mut spec = Spec::new(&opts.id, &opts.kernel, &opts.initfs, &opts.rootfs);
    if !opts.argv.is_empty() {
        spec = spec.argv(opts.argv.clone());
    }
    if let Some(c) = opts.cpus {
        spec = spec.cpus(c);
    }
    if let Some(m) = opts.memory_mib {
        spec = spec.memory_mib(m);
    }
    if let Some(cwd) = opts.cwd.clone() {
        spec = spec.cwd(cwd);
    }
    spec = spec.networking(opts.network).tty(opts.tty);
    if let Some(octet) = opts.ip_octet {
        spec = spec.ip_octet(octet);
    }
    // Surface the effective IP so users can predict/override it.
    if opts.network {
        let octet = opts
            .ip_octet
            .unwrap_or_else(|| hephaestus_vmm::allocate_ip_octet(&opts.id));
        eprintln!("hephaestus: guest IP 192.168.64.{octet}/24 (id={})", opts.id);
    }

    let stdio = TerminalSink::default();
    let vm = match spec.build(stdio) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("hephaestus: new: {e}");
            return ExitCode::from(1);
        }
    };

    eprintln!("hephaestus: booting VM…");
    if let Err(e) = vm.create() {
        eprintln!("hephaestus: create: {e}");
        return ExitCode::from(1);
    }

    eprintln!("hephaestus: starting container process…");
    if let Err(e) = vm.start() {
        eprintln!("hephaestus: start: {e}");
        return ExitCode::from(1);
    }

    let exit_code = match vm.wait() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hephaestus: wait: {e}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = vm.stop() {
        eprintln!("hephaestus: stop: {e}");
    }

    eprintln!("hephaestus: guest exited with code {exit_code}");
    if let Ok(code) = u8::try_from(exit_code) {
        ExitCode::from(code)
    } else {
        ExitCode::from(1)
    }
}

const ROOTFS_USAGE: &str = "\
usage: hephaestus rootfs --from-tar <path> --output <path.ext4> \\
    [--size-mib N] [--compression auto|none|gzip|zstd]";

#[derive(Debug)]
struct RootfsOptions {
    tar: PathBuf,
    output: PathBuf,
    size_mib: u64,
    compression: Option<Compression>, // None → auto-detect
}

fn parse_rootfs_args(args: &mut impl Iterator<Item = String>) -> Result<RootfsOptions, String> {
    let mut opts = RootfsOptions {
        tar: PathBuf::new(),
        output: PathBuf::new(),
        size_mib: 512,
        compression: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from-tar" => opts.tar = require_value(args, "--from-tar")?.into(),
            "--output" => opts.output = require_value(args, "--output")?.into(),
            "--size-mib" => {
                opts.size_mib = require_value(args, "--size-mib")?
                    .parse()
                    .map_err(|e| format!("invalid --size-mib: {e}"))?;
            }
            "--compression" => {
                opts.compression = Some(match require_value(args, "--compression")?.as_str() {
                    "auto" => return Err("--compression=auto is the default; omit the flag".into()),
                    "none" => Compression::None,
                    "gzip" => Compression::Gzip,
                    "zstd" => Compression::Zstd,
                    other => return Err(format!("unknown compression `{other}`")),
                });
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if opts.tar.as_os_str().is_empty() {
        return Err("missing --from-tar".into());
    }
    if opts.output.as_os_str().is_empty() {
        return Err("missing --output".into());
    }
    if !opts.tar.exists() {
        return Err(format!("--from-tar path does not exist: {}", opts.tar.display()));
    }
    Ok(opts)
}

fn rootfs(opts: RootfsOptions) -> ExitCode {
    let compression = match opts.compression {
        Some(c) => c,
        None => match Compression::auto_detect(&opts.tar) {
            Ok(Some(c)) => {
                eprintln!("hephaestus: auto-detected compression: {c:?}");
                c
            }
            Ok(None) => {
                eprintln!("hephaestus: no known compression header; assuming plain tar");
                Compression::None
            }
            Err(e) => {
                eprintln!("hephaestus: cannot read --from-tar: {e}");
                return ExitCode::from(1);
            }
        },
    };

    eprintln!(
        "hephaestus: unpacking {} → {} ({} MiB, {:?})",
        opts.tar.display(),
        opts.output.display(),
        opts.size_mib,
        compression
    );

    match build_rootfs_from_tar(&opts.tar, &opts.output, opts.size_mib, compression) {
        Ok(()) => {
            eprintln!("hephaestus: wrote {}", opts.output.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hephaestus: rootfs build failed: {e}");
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// vz-boot subcommand — direct Virtualization.framework smoke test (N0 spike).
// =============================================================================

const VZ_BOOT_USAGE: &str = "\
usage: hephaestus vz-boot \\
    --kernel <path> \\
    --rootfs <path> \\
    --log <output-path> \\
    [--cpus N] [--memory-mib N] [--run-seconds N]";

#[derive(Debug)]
struct VzBootOptions {
    kernel: PathBuf,
    rootfs: PathBuf,
    log: PathBuf,
    cpus: u32,
    memory_mib: u64,
    run_seconds: u32,
}

fn parse_vz_boot_args(args: &mut impl Iterator<Item = String>) -> Result<VzBootOptions, String> {
    let mut opts = VzBootOptions {
        kernel: PathBuf::new(),
        rootfs: PathBuf::new(),
        log: PathBuf::new(),
        cpus: 0,
        memory_mib: 0,
        run_seconds: 0,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kernel" => opts.kernel = require_value(args, "--kernel")?.into(),
            "--rootfs" => opts.rootfs = require_value(args, "--rootfs")?.into(),
            "--log" => opts.log = require_value(args, "--log")?.into(),
            "--cpus" => {
                opts.cpus = require_value(args, "--cpus")?
                    .parse()
                    .map_err(|e| format!("invalid --cpus: {e}"))?;
            }
            "--memory-mib" => {
                opts.memory_mib = require_value(args, "--memory-mib")?
                    .parse()
                    .map_err(|e| format!("invalid --memory-mib: {e}"))?;
            }
            "--run-seconds" => {
                opts.run_seconds = require_value(args, "--run-seconds")?
                    .parse()
                    .map_err(|e| format!("invalid --run-seconds: {e}"))?;
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if opts.kernel.as_os_str().is_empty() {
        return Err("missing --kernel".into());
    }
    if opts.rootfs.as_os_str().is_empty() {
        return Err("missing --rootfs".into());
    }
    if opts.log.as_os_str().is_empty() {
        return Err("missing --log".into());
    }
    for (label, path) in [("--kernel", &opts.kernel), ("--rootfs", &opts.rootfs)] {
        if !path.exists() {
            return Err(format!("{label} path does not exist: {}", path.display()));
        }
    }
    Ok(opts)
}

fn vz_boot_cmd(opts: VzBootOptions) -> ExitCode {
    eprintln!(
        "hephaestus: vz-boot kernel={} rootfs={} log={} (cpus={} mem={} MiB run={} s)",
        opts.kernel.display(),
        opts.rootfs.display(),
        opts.log.display(),
        if opts.cpus == 0 { "default".into() } else { opts.cpus.to_string() },
        if opts.memory_mib == 0 { "default".into() } else { opts.memory_mib.to_string() },
        if opts.run_seconds == 0 { "default".into() } else { opts.run_seconds.to_string() },
    );
    let start = std::time::Instant::now();
    match vz_boot(
        &opts.kernel,
        &opts.rootfs,
        &opts.log,
        opts.cpus,
        opts.memory_mib,
        opts.run_seconds,
    ) {
        Ok(()) => {
            eprintln!("hephaestus: vz-boot completed in {:?}", start.elapsed());
            eprintln!("hephaestus: inspect the guest serial log at {}", opts.log.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hephaestus: vz-boot: {e}");
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// vz-sh subcommand — interactive shell on the direct-VZ path (N2.1).
// =============================================================================

const VZ_SH_USAGE: &str = "\
usage: hephaestus vz-sh \\
    --kernel <path> --rootfs <path> \\
    [--cpus N] [--memory-mib N] [--timeout-seconds N]";

#[derive(Debug)]
struct VzShOptions {
    kernel: PathBuf,
    rootfs: PathBuf,
    cpus: u32,
    memory_mib: u64,
    timeout_seconds: u32,
}

fn parse_vz_sh_args(args: &mut impl Iterator<Item = String>) -> Result<VzShOptions, String> {
    let mut opts = VzShOptions {
        kernel: PathBuf::new(),
        rootfs: PathBuf::new(),
        cpus: 0,
        memory_mib: 0,
        timeout_seconds: 0,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kernel" => opts.kernel = require_value(args, "--kernel")?.into(),
            "--rootfs" => opts.rootfs = require_value(args, "--rootfs")?.into(),
            "--cpus" => {
                opts.cpus = require_value(args, "--cpus")?
                    .parse()
                    .map_err(|e| format!("invalid --cpus: {e}"))?;
            }
            "--memory-mib" => {
                opts.memory_mib = require_value(args, "--memory-mib")?
                    .parse()
                    .map_err(|e| format!("invalid --memory-mib: {e}"))?;
            }
            "--timeout-seconds" => {
                opts.timeout_seconds = require_value(args, "--timeout-seconds")?
                    .parse()
                    .map_err(|e| format!("invalid --timeout-seconds: {e}"))?;
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if opts.kernel.as_os_str().is_empty() {
        return Err("missing --kernel".into());
    }
    if opts.rootfs.as_os_str().is_empty() {
        return Err("missing --rootfs".into());
    }
    for (label, path) in [("--kernel", &opts.kernel), ("--rootfs", &opts.rootfs)] {
        if !path.exists() {
            return Err(format!("{label} path does not exist: {}", path.display()));
        }
    }
    Ok(opts)
}

fn vz_sh_cmd(opts: VzShOptions) -> ExitCode {
    eprintln!("hephaestus: vz-sh (exit shell with `exit` or Ctrl-D)");
    match vz_sh(
        &opts.kernel,
        &opts.rootfs,
        opts.cpus,
        opts.memory_mib,
        opts.timeout_seconds,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hephaestus: vz-sh: {e}");
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// vz-snapshot subcommand — direct Virtualization.framework save/restore (N1).
// =============================================================================

const VZ_SNAP_USAGE: &str = "\
usage:
  hephaestus vz-snapshot save \\
      --kernel <path> --rootfs <path> --log <path> --save <path> \\
      [--cpus N] [--memory-mib N] [--settle-seconds N]
  hephaestus vz-snapshot restore \\
      --kernel <path> --rootfs <path> --log <path> --save <path> \\
      [--cpus N] [--memory-mib N] [--run-seconds N]";

#[derive(Debug)]
struct VzSnapOptions {
    kernel: PathBuf,
    rootfs: PathBuf,
    log: PathBuf,
    save: PathBuf,
    cpus: u32,
    memory_mib: u64,
    seconds: u32, // settle (save) or run (restore)
}

fn parse_vz_snap_args(
    args: &mut impl Iterator<Item = String>,
    saving: bool,
) -> Result<VzSnapOptions, String> {
    let mut opts = VzSnapOptions {
        kernel: PathBuf::new(),
        rootfs: PathBuf::new(),
        log: PathBuf::new(),
        save: PathBuf::new(),
        cpus: 0,
        memory_mib: 0,
        seconds: 0,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kernel" => opts.kernel = require_value(args, "--kernel")?.into(),
            "--rootfs" => opts.rootfs = require_value(args, "--rootfs")?.into(),
            "--log" => opts.log = require_value(args, "--log")?.into(),
            "--save" => opts.save = require_value(args, "--save")?.into(),
            "--cpus" => {
                opts.cpus = require_value(args, "--cpus")?
                    .parse()
                    .map_err(|e| format!("invalid --cpus: {e}"))?;
            }
            "--memory-mib" => {
                opts.memory_mib = require_value(args, "--memory-mib")?
                    .parse()
                    .map_err(|e| format!("invalid --memory-mib: {e}"))?;
            }
            "--settle-seconds" | "--run-seconds" => {
                opts.seconds = require_value(args, &arg)?
                    .parse()
                    .map_err(|e| format!("invalid seconds value: {e}"))?;
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    for (label, path, must_exist) in [
        ("--kernel", &opts.kernel, true),
        ("--rootfs", &opts.rootfs, true),
        ("--log", &opts.log, false),
        ("--save", &opts.save, !saving), // save file must exist on restore
    ] {
        if path.as_os_str().is_empty() {
            return Err(format!("missing {label}"));
        }
        if must_exist && !path.exists() {
            return Err(format!("{label} path does not exist: {}", path.display()));
        }
    }
    Ok(opts)
}

fn vz_snap_save_cmd(opts: VzSnapOptions) -> ExitCode {
    eprintln!(
        "hephaestus: vz-snapshot save → {} (settle={}s)",
        opts.save.display(),
        if opts.seconds == 0 { 3 } else { opts.seconds }
    );
    let start = std::time::Instant::now();
    match vz_snapshot_save(
        &opts.kernel,
        &opts.rootfs,
        &opts.log,
        &opts.save,
        opts.cpus,
        opts.memory_mib,
        opts.seconds,
    ) {
        Ok(()) => {
            let size = std::fs::metadata(&opts.save).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "hephaestus: saved {} bytes in {:?} (includes settle time)",
                size,
                start.elapsed()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hephaestus: vz-snapshot save: {e}");
            ExitCode::from(1)
        }
    }
}

fn vz_snap_restore_cmd(opts: VzSnapOptions) -> ExitCode {
    eprintln!(
        "hephaestus: vz-snapshot restore ← {} (run={}s)",
        opts.save.display(),
        if opts.seconds == 0 { 3 } else { opts.seconds }
    );
    let wall_start = std::time::Instant::now();
    match vz_snapshot_restore(
        &opts.kernel,
        &opts.rootfs,
        &opts.log,
        &opts.save,
        opts.cpus,
        opts.memory_mib,
        opts.seconds,
    ) {
        Ok(restore_nanos) => {
            eprintln!(
                "hephaestus: restore+resume took {:.3} ms (wall clock incl. run: {:?})",
                restore_nanos as f64 / 1_000_000.0,
                wall_start.elapsed()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hephaestus: vz-snapshot restore: {e}");
            ExitCode::from(1)
        }
    }
}

/// Streams guest stdout/stderr to the host terminal. The bridge calls these
/// from arbitrary Swift threads, so we guard stdout/stderr with a mutex to
/// avoid interleaved writes within a single chunk.
#[derive(Default)]
struct TerminalSink {
    lock: Mutex<()>,
}

impl StdioSink for TerminalSink {
    fn on_stdout(&self, bytes: &[u8]) {
        let _g = self.lock.lock().unwrap();
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
    fn on_stderr(&self, bytes: &[u8]) {
        let _g = self.lock.lock().unwrap();
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(bytes);
        let _ = err.flush();
    }
}

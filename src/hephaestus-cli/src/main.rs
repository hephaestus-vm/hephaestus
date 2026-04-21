use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use hephaestus_vmm::{Spec, StdioSink};

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
        Some(other) => {
            eprintln!("hephaestus: unknown subcommand `{other}`");
            eprintln!("usage: hephaestus <ping|run>");
            ExitCode::from(2)
        }
        None => {
            eprintln!("usage: hephaestus <ping|run>");
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

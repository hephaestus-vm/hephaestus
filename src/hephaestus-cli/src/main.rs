use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("ping") => {
            println!("{}", hephaestus_vmm::ping());
            ExitCode::SUCCESS
        }
        Some("new") => match parse_new_args(&mut args) {
            Ok((id, kernel, rootfs)) => match hephaestus_vmm::Vm::new(&id, &kernel, &rootfs) {
                Ok(vm) => {
                    println!("hephaestus: constructed VM handle id={id}");
                    drop(vm);
                    println!("hephaestus: dropped VM handle");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("hephaestus: {e}");
                    ExitCode::from(1)
                }
            },
            Err(msg) => {
                eprintln!("hephaestus: {msg}");
                eprintln!("usage: hephaestus new --id <id> --kernel <path> --rootfs <path>");
                ExitCode::from(2)
            }
        },
        Some(other) => {
            eprintln!("hephaestus: unknown subcommand `{other}`");
            eprintln!("usage: hephaestus <ping|new>");
            ExitCode::from(2)
        }
        None => {
            eprintln!("usage: hephaestus <ping|new>");
            ExitCode::from(2)
        }
    }
}

fn parse_new_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, PathBuf, PathBuf), String> {
    let mut id: Option<String> = None;
    let mut kernel: Option<PathBuf> = None;
    let mut rootfs: Option<PathBuf> = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--id" => id = Some(value),
            "--kernel" => kernel = Some(PathBuf::from(value)),
            "--rootfs" => rootfs = Some(PathBuf::from(value)),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok((
        id.unwrap_or_else(|| "hephaestus-vm".into()),
        kernel.ok_or("missing --kernel")?,
        rootfs.ok_or("missing --rootfs")?,
    ))
}

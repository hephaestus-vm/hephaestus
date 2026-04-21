use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("ping") => {
            println!("{}", hephaestus_vmm::ping());
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("hephaestus: unknown subcommand `{other}`");
            eprintln!("usage: hephaestus <ping>");
            ExitCode::from(2)
        }
        None => {
            eprintln!("usage: hephaestus <ping>");
            ExitCode::from(2)
        }
    }
}

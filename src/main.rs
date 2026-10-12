mod cli;
mod ui;

use clap::Parser;

/// Scan documents from eSCL (AirScan) network scanners.
///
/// Without a command, the graphical interface starts.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<cli::Command>,
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    match cli.command {
        Some(command) => match cli::run(command) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                std::process::ExitCode::FAILURE
            }
        },
        None => run_gui(),
    }
}

fn run_gui() -> std::process::ExitCode {
    std::process::ExitCode::from(ui::run().get())
}

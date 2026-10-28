//! The one dvb binary: `engine | run | desk | <owner verb>`.

use clap::{Parser, Subcommand};
use dvb_cli::{Target, Verb};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "dvb",
    version,
    about = "dvb: a team of coding agents, run on your machine"
)]
struct Cli {
    #[command(flatten)]
    target: Target,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the engine (a system service; `--dev <dir>` for a local engine).
    Engine,
    /// Inside a run: `dvb run exec <run>`, or `dvb run <verb> [<json>]`.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Serve the app on 127.0.0.1 and print its launch link.
    Desk {
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Internal detached browser startup supervisor; the launch URL comes on stdin.
    #[command(hide = true)]
    AppLaunch {
        #[arg(long)]
        helper: Option<std::path::PathBuf>,
        #[arg(long)]
        browser: Option<std::path::PathBuf>,
    },
    #[command(flatten)]
    Verb(Verb),
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))
}

fn code(c: i32) -> ExitCode {
    ExitCode::from(u8::try_from(c).unwrap_or(1))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { args } => match runtime() {
            Ok(rt) => rt.block_on(dvb_run::main(&args)),
            Err(error) => {
                eprintln!("dvb run: {error}");
                std::process::Termination::report(dvb_core::RunOutcome::SetupFailed)
            }
        },
        Cmd::AppLaunch { helper, browser } => {
            let mut url = String::new();
            let launched = match std::io::stdin().read_line(&mut url) {
                Ok(_) => dvb_cli::app::supervise_launch(
                    url.trim_end(),
                    helper.as_deref(),
                    browser.as_deref(),
                    &mut std::io::stdout(),
                ),
                Err(e) => Err(dvb_cli::client::CliError::Message(e.to_string())),
            };
            match launched {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dvb app: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Engine => {
            let cfg = match &cli.target.dev {
                Some(dir) => match std::env::current_exe() {
                    Ok(bin) => Ok(dvb_engine::Config::dev(dir, bin)),
                    Err(e) => Err(e.to_string()),
                },
                None => dvb_engine::Config::install().map_err(|e| e.to_string()),
            };
            let result = match cfg {
                Ok(cfg) => runtime()
                    .and_then(|rt| rt.block_on(dvb_engine::run(cfg)).map_err(|e| e.to_string())),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dvb engine: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Desk { port } => match runtime() {
            Ok(rt) => match rt.block_on(dvb_desk::serve(cli.target.sock(), port)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("dvb desk: {error}");
                    ExitCode::FAILURE
                }
            },
            Err(error) => {
                eprintln!("dvb desk: {error}");
                ExitCode::FAILURE
            }
        },
        Cmd::Verb(verb) => match runtime() {
            Ok(rt) => code(rt.block_on(dvb_cli::main(&cli.target, verb))),
            Err(e) => {
                eprintln!("dvb: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

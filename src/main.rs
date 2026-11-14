use std::process::ExitCode;

use clap::{CommandFactory, Parser};

fn main() -> ExitCode {
    let args = comrade::app::Args::parse();
    if let Some(shell) = args.completions {
        let mut cmd = comrade::app::Args::command();
        clap_complete::generate(shell, &mut cmd, "comrade", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }
    if args.man {
        let cmd = comrade::app::Args::command();
        return match clap_mangen::Man::new(cmd).render(&mut std::io::stdout()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("comrade: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match comrade::app::run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("comrade: {e:#}");
            ExitCode::FAILURE
        }
    }
}

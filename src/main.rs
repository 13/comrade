use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = comrade::app::Args::parse();
    match comrade::app::run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("comrade: {e:#}");
            ExitCode::FAILURE
        }
    }
}

//! Thin binary shell: parse args, assemble the real adapters, dispatch to the
//! library, and map a returned error to a process exit code printed once. No
//! logic lives here, so the integration tests drive everything through the
//! library's public surface.

use std::process::ExitCode;

use clap::Parser;

use hort::{Cli, RealDeps, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match RealDeps::assemble().and_then(|deps| run(cli, &deps)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

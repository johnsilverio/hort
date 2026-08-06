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
        // A run that opened a session leaves with what the session exited with,
        // so a caller can tell what ran inside the sandbox from what hort did.
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

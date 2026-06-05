//! Thin binary shell (ADR-0002): parse args, assemble the real adapters,
//! dispatch to the library, and map a returned `HortError` to a process exit
//! code + its canonical message printed once. No logic lives here.

fn main() {
    // TODO(CLI-02): Cli::parse() -> RealDeps::assemble() -> hort::run(cli, &deps)
    //               -> map HortError to a process exit code (ARCH thin-main shell).
}

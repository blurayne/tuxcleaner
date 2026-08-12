use anyhow::Result;
use clap::Parser;
use tuxcleaner::cli::{Cli, run};

fn main() -> Result<()> {
    run(Cli::parse())
}

use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand};
use dialoguer::{Confirm, MultiSelect, theme::ColorfulTheme};
use serde_json::json;

use crate::analyze::{AnalysisReport, analyze};
use crate::distro::Distribution;
use crate::executor::Executor;
use crate::history::{HistoryRecord, HistoryStore};
use crate::model::{ActionResult, CleanupGroup, ScanReport};
use crate::purge::{PurgeCandidate, scan_artifacts};
use crate::scanner::Scanner;
use crate::size::{format_bytes, parse_size};
use crate::uninstall::{Application, ApplicationCatalog, ApplicationReport, ApplicationSource};
use crate::update;
use crate::{status, tui};

mod analyze;
mod args;
mod clean;
mod information;
mod purge;
mod support;
mod uninstall;

use analyze::*;
pub use args::*;
use clean::*;
use information::*;
use purge::*;
use support::*;
use uninstall::*;

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(command) => run_command(command),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => tui::interactive_app(),
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub fn run_command(command: Commands) -> Result<()> {
    match command {
        Commands::Clean(args) => run_clean(args),
        Commands::Uninstall(args) => run_uninstall(args),
        Commands::Analyze(args) => run_analyze(args),
        Commands::Purge(args) => run_purge(args),
        Commands::Status(args) => run_status(args),
        Commands::History(args) => run_history(args),
        Commands::Update(args) => run_update(args),
    }
}

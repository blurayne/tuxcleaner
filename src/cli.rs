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
use crate::update;
use crate::{status, tui};

#[derive(Debug, Parser)]
#[command(
    name = "tuxcleaner",
    version,
    about = "A safety-first Linux cleanup and disk analysis toolkit",
    after_help = "Run without a subcommand to open the interactive menu. Destructive commands always require a selection or --yes."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Scan known caches and clean explicitly selected groups
    Clean(CleanArgs),
    /// Analyze disk usage and find large files without deleting anything
    Analyze(AnalyzeArgs),
    /// Find and remove old project build artifacts
    Purge(PurgeArgs),
    /// Show a read-only system health snapshot
    Status(StatusArgs),
    /// Review previous cleanup operations
    History(HistoryArgs),
    /// Check for or install a verified GitHub release
    Update(UpdateArgs),
}

#[derive(Debug, Clone, Args, Default)]
pub struct CleanArgs {
    /// Preview every selected operation without changing the system
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm every requested group non-interactively
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Restrict cleanup to one or more groups
    #[arg(long, value_enum, value_delimiter = ',')]
    pub groups: Vec<CleanupGroup>,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct AnalyzeArgs {
    /// Directory to analyze, defaults to the current user's home
    pub path: Option<PathBuf>,
    /// Minimum large-file size, for example 500M or 1GiB
    #[arg(long, default_value = "500M")]
    pub min_size: String,
    /// Maximum traversal depth
    #[arg(long, default_value_t = 20)]
    pub max_depth: usize,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

impl Default for AnalyzeArgs {
    fn default() -> Self {
        Self {
            path: None,
            min_size: "500M".into(),
            max_depth: 20,
            json: false,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct PurgeArgs {
    /// Project roots to scan; may be repeated
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Only include artifacts this many days old
    #[arg(long, default_value_t = 30)]
    pub older_than_days: u64,
    /// Preview selected removals without changing the filesystem
    #[arg(long)]
    pub dry_run: bool,
    /// Select every matching artifact non-interactively
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

impl Default for PurgeArgs {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            older_than_days: 30,
            dry_run: false,
            yes: false,
            json: false,
        }
    }
}

#[derive(Debug, Clone, Args, Default)]
pub struct StatusArgs {
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct HistoryArgs {
    /// Maximum number of entries to show
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args, Default)]
pub struct UpdateArgs {
    /// Check the selected release without installing it
    #[arg(long)]
    pub check: bool,
    /// Preview the update without replacing the current binary
    #[arg(long)]
    pub dry_run: bool,
    /// Install a specific release, for example 0.2.0
    #[arg(long = "version", value_name = "VERSION")]
    pub target_version: Option<String>,
    /// Confirm the update non-interactively
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

impl Default for HistoryArgs {
    fn default() -> Self {
        Self {
            limit: 20,
            json: false,
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(command) => run_command(command),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            if let Some(action) = tui::interactive_menu()? {
                run_command(match action {
                    tui::MenuAction::Clean => Commands::Clean(CleanArgs::default()),
                    tui::MenuAction::Analyze => Commands::Analyze(AnalyzeArgs::default()),
                    tui::MenuAction::Purge => Commands::Purge(PurgeArgs::default()),
                    tui::MenuAction::Status => Commands::Status(StatusArgs::default()),
                    tui::MenuAction::History => Commands::History(HistoryArgs::default()),
                    tui::MenuAction::Update => Commands::Update(UpdateArgs::default()),
                })
            } else {
                Ok(())
            }
        }
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
        Commands::Analyze(args) => run_analyze(args),
        Commands::Purge(args) => run_purge(args),
        Commands::Status(args) => run_status(args),
        Commands::History(args) => run_history(args),
        Commands::Update(args) => run_update(args),
    }
}

fn run_clean(args: CleanArgs) -> Result<()> {
    validate_noninteractive(args.yes, args.json, "clean")?;
    let home = home_dir()?;
    let distro = Distribution::detect()?;
    let report = Scanner::new(home.clone(), distro.clone()).scan();
    if args.json && !args.yes {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"scan": report, "results": []}))?
        );
        return Ok(());
    }
    let groups = choose_groups(&report, &args)?;
    let selected: Vec<_> = report
        .items
        .iter()
        .filter(|item| groups.contains(&item.group))
        .cloned()
        .collect();

    if selected.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"scan": report, "results": []}))?
            );
        } else {
            print_scan_report(&report);
            println!("Nothing selected.");
        }
        return Ok(());
    }

    if !args.json {
        print_scan_report(&report);
        println!();
        println!(
            "{} {} item(s).",
            if args.dry_run {
                "Previewing"
            } else {
                "Cleaning"
            },
            selected.len()
        );
    }
    let executor = Executor::new(home);
    let results: Vec<_> = selected
        .iter()
        .map(|item| executor.execute(item, args.dry_run))
        .collect();
    record_history(&distro.name, "clean", &results)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"scan": report, "results": results}))?
        );
    } else {
        print_results(&results);
    }
    fail_if_actions_failed(&results)
}

fn choose_groups(report: &ScanReport, args: &CleanArgs) -> Result<HashSet<CleanupGroup>> {
    if !args.groups.is_empty() {
        return Ok(args.groups.iter().copied().collect());
    }
    let available: Vec<_> = report
        .groups
        .iter()
        .filter(|summary| summary.item_count > 0)
        .collect();
    if args.yes {
        return Ok(available.iter().map(|summary| summary.group).collect());
    }
    if available.is_empty() {
        return Ok(HashSet::new());
    }
    let labels: Vec<_> = available
        .iter()
        .map(|summary| {
            format!(
                "{}: {} across {} item(s)",
                summary.group,
                format_bytes(summary.estimated_bytes),
                summary.item_count
            )
        })
        .collect();
    let selection = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select cleanup groups (nothing is selected by default)")
        .items(&labels)
        .interact()?;
    Ok(selection
        .into_iter()
        .map(|index| available[index].group)
        .collect())
}

fn run_analyze(args: AnalyzeArgs) -> Result<()> {
    let root = args.path.unwrap_or(home_dir()?);
    let minimum_size = parse_size(&args.min_size)?;
    let report = analyze(&root, minimum_size, args.max_depth)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_analysis(&report);
    }
    Ok(())
}

fn run_purge(args: PurgeArgs) -> Result<()> {
    validate_noninteractive(args.yes, args.json, "purge")?;
    let home = home_dir()?;
    let distro = Distribution::detect()?;
    let roots = if args.paths.is_empty() {
        default_project_roots(&home)?
    } else {
        args.paths.clone()
    };
    let candidates = scan_artifacts(&roots, args.older_than_days);
    if args.json && !args.yes {
        println!("{}", serde_json::to_string_pretty(&candidates)?);
        return Ok(());
    }
    if !args.json {
        print_purge_candidates(&candidates, &roots, args.older_than_days);
    }
    if candidates.is_empty() {
        return Ok(());
    }

    let selected = if args.yes {
        candidates.clone()
    } else {
        choose_artifacts(&candidates)?
    };
    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }
    if !args.yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Permanently remove {} selected artifact(s)?",
                selected.len()
            ))
            .default(false)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    let executor = Executor::new(home);
    let results: Vec<_> = selected
        .iter()
        .map(PurgeCandidate::cleanup_item)
        .map(|item| executor.execute(&item, args.dry_run))
        .collect();
    record_history(&distro.name, "purge", &results)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"candidates": candidates, "results": results}))?
        );
    } else {
        print_results(&results);
    }
    fail_if_actions_failed(&results)
}

fn choose_artifacts(candidates: &[PurgeCandidate]) -> Result<Vec<PurgeCandidate>> {
    let labels: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} | {} | {} days old",
                candidate.path.display(),
                format_bytes(candidate.size),
                candidate.age_days
            )
        })
        .collect();
    let selection = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select project artifacts to permanently remove")
        .items(&labels)
        .interact()?;
    Ok(selection
        .into_iter()
        .map(|index| candidates[index].clone())
        .collect())
}

fn run_status(args: StatusArgs) -> Result<()> {
    let report = status::collect()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("TuxCleaner status for {}", report.hostname);
        println!(
            "CPU: {} logical cores | load {:.2} / {:.2} / {:.2}",
            report.logical_cpus,
            report.load_average[0],
            report.load_average[1],
            report.load_average[2]
        );
        println!(
            "Memory: {} / {} used ({:.1}%)",
            format_bytes(report.memory.used_bytes),
            format_bytes(report.memory.total_bytes),
            report.memory.used_percent
        );
        println!("Uptime: {}", format_duration(report.uptime_seconds));
        println!();
        println!("Disks:");
        for disk in report.disks {
            println!(
                "  {:<20} {:>8} / {:>8} ({:>5.1}%)  {}",
                disk.mount,
                format_bytes(disk.used_bytes),
                format_bytes(disk.total_bytes),
                disk.used_percent,
                disk.filesystem
            );
        }
    }
    Ok(())
}

fn run_history(args: HistoryArgs) -> Result<()> {
    let store = HistoryStore::system_default()?;
    let records = store.read_recent(args.limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else if records.is_empty() {
        println!("No cleanup history at {}.", store.path().display());
    } else {
        for record in records {
            let succeeded = record
                .results
                .iter()
                .filter(|result| result.success)
                .count();
            let reclaimed: u64 = record
                .results
                .iter()
                .filter(|result| result.success && !result.dry_run)
                .map(|result| result.estimated_bytes)
                .sum();
            println!(
                "{}  {:<6}  {}/{} succeeded  estimated {}",
                record.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
                record.command,
                succeeded,
                record.results.len(),
                format_bytes(reclaimed)
            );
        }
    }
    Ok(())
}

fn run_update(args: UpdateArgs) -> Result<()> {
    if !args.check && !args.dry_run && !args.yes && !io::stdin().is_terminal() {
        bail!("update needs an interactive terminal; use --dry-run, --check, or --yes")
    }
    let info = update::check(args.target_version.as_deref())?;
    if args.check || args.dry_run || (!info.update_available && args.target_version.is_none()) {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&info)?);
        } else if info.update_available || args.target_version.is_some() {
            println!(
                "TuxCleaner {} is installed; release {} is available for {}{}.",
                info.current_version,
                info.available_version,
                info.target,
                if args.dry_run { " (dry-run)" } else { "" }
            );
        } else {
            println!("TuxCleaner {} is already up to date.", info.current_version);
        }
        return Ok(());
    }
    if !args.yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Replace TuxCleaner {} with {}?",
                info.current_version, info.available_version
            ))
            .default(false)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }
    let result = update::install(args.target_version.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.updated {
        println!(
            "Updated TuxCleaner from {} to {} for {}.",
            result.previous_version, result.installed_version, result.target
        );
    } else {
        println!(
            "TuxCleaner {} is already up to date.",
            result.installed_version
        );
    }
    Ok(())
}

fn validate_noninteractive(yes: bool, json: bool, command: &str) -> Result<()> {
    if !yes && !io::stdin().is_terminal() && !json {
        bail!(
            "{command} needs an interactive terminal; use --dry-run --yes to preview or --yes to confirm"
        )
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(std::fs::canonicalize(&home).unwrap_or(home))
}

fn default_project_roots(home: &std::path::Path) -> Result<Vec<PathBuf>> {
    let names = ["Projects", "Progetti", "GitHub", "Code", "dev"];
    let roots: Vec<_> = names
        .iter()
        .map(|name| home.join(name))
        .filter(|path| path.is_dir())
        .collect();
    if roots.is_empty() {
        Ok(vec![
            std::env::current_dir().context("failed to determine current directory")?,
        ])
    } else {
        Ok(roots)
    }
}

fn record_history(distribution: &str, command: &str, results: &[ActionResult]) -> Result<()> {
    HistoryStore::system_default()?.append(&HistoryRecord {
        timestamp: Utc::now(),
        distribution: distribution.into(),
        command: command.into(),
        results: results.to_vec(),
    })
}

fn fail_if_actions_failed(results: &[ActionResult]) -> Result<()> {
    let failed = results.iter().filter(|result| !result.success).count();
    if failed > 0 {
        bail!("{failed} cleanup operation(s) failed; successful operations were not rolled back")
    }
    Ok(())
}

fn print_scan_report(report: &ScanReport) {
    println!("TuxCleaner scan on {}", report.distribution);
    println!();
    for summary in &report.groups {
        println!(
            "  {:<24} {:>10}  {} item(s)",
            summary.group,
            format_bytes(summary.estimated_bytes),
            summary.item_count
        );
    }
    println!(
        "  {:<24} {:>10}",
        "Estimated total",
        format_bytes(report.estimated_total_bytes)
    );
    for warning in &report.warnings {
        println!("Warning: {warning}");
    }
}

fn print_results(results: &[ActionResult]) {
    println!();
    for result in results {
        let marker = if result.success { "OK" } else { "FAIL" };
        let mode = if result.dry_run { " [dry-run]" } else { "" };
        println!("  [{marker}] {}{mode}: {}", result.label, result.message);
    }
    let estimated: u64 = results
        .iter()
        .filter(|result| result.success)
        .map(|result| result.estimated_bytes)
        .sum();
    println!("Estimated space covered: {}", format_bytes(estimated));
}

fn print_analysis(report: &AnalysisReport) {
    println!("Disk analysis: {}", report.root.display());
    println!(
        "{} across {} files ({} entries skipped)",
        format_bytes(report.total_size),
        report.total_files,
        report.skipped_entries
    );
    println!();
    println!("Top-level usage:");
    for entry in report.entries.iter().take(30) {
        println!(
            "  {:>10}  {}",
            format_bytes(entry.size),
            entry.path.display()
        );
    }
    println!();
    println!("Large personal files:");
    let personal: Vec<_> = report
        .large_files
        .iter()
        .filter(|file| !file.app_data)
        .collect();
    if personal.is_empty() {
        println!("  None above the selected threshold.");
    } else {
        for file in personal.iter().take(50) {
            println!(
                "  {:>10}  {}  {}",
                format_bytes(file.size),
                format_timestamp(file.modified_unix),
                file.path.display()
            );
        }
    }
    let app_data: Vec<_> = report
        .large_files
        .iter()
        .filter(|file| file.app_data)
        .collect();
    if !app_data.is_empty() {
        println!();
        println!(
            "Application data (review carefully; removal can break or erase application data):"
        );
        for file in app_data.iter().take(50) {
            println!("  {:>10}  {}", format_bytes(file.size), file.path.display());
        }
    }
}

fn print_purge_candidates(candidates: &[PurgeCandidate], roots: &[PathBuf], age: u64) {
    println!("Project artifact scan (older than {age} days)");
    println!(
        "Roots: {}",
        roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if candidates.is_empty() {
        println!("No matching artifacts found.");
        return;
    }
    for candidate in candidates {
        println!(
            "  {:>10}  {:>4} days  {}",
            format_bytes(candidate.size),
            candidate.age_days,
            candidate.path.display()
        );
    }
}

fn format_timestamp(timestamp: Option<u64>) -> String {
    timestamp
        .and_then(|value| DateTime::<Utc>::from_timestamp(value as i64, 0))
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    format!("{days}d {hours}h {minutes}m")
}

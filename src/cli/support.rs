use super::*;

pub(super) fn validate_noninteractive(yes: bool, json: bool, command: &str) -> Result<()> {
    if !yes && !io::stdin().is_terminal() && !json {
        bail!(
            "{command} needs an interactive terminal; use --dry-run --yes to preview or --yes to confirm"
        )
    }
    Ok(())
}

pub(super) fn home_dir() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(std::fs::canonicalize(&home).unwrap_or(home))
}

pub(super) fn default_project_roots(home: &std::path::Path) -> Result<Vec<PathBuf>> {
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

pub(super) fn record_history(distribution: &str, command: &str, results: &[ActionResult]) {
    let outcome = HistoryStore::system_default().and_then(|store| {
        store.append(&HistoryRecord {
            timestamp: Utc::now(),
            distribution: distribution.into(),
            command: command.into(),
            results: results.to_vec(),
        })
    });
    if let Err(error) = outcome {
        eprintln!("Warning: the operation completed, but history could not be recorded: {error:#}");
    }
}

pub(super) fn fail_if_actions_failed(results: &[ActionResult]) -> Result<()> {
    let failed = results.iter().filter(|result| !result.success).count();
    if failed > 0 {
        bail!("{failed} cleanup operation(s) failed; successful operations were not rolled back")
    }
    Ok(())
}

pub(super) fn print_scan_report(report: &ScanReport) {
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

pub(super) fn print_results(results: &[ActionResult]) {
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

pub(super) fn print_analysis(report: &AnalysisReport) {
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

pub(super) fn print_purge_candidates(candidates: &[PurgeCandidate], roots: &[PathBuf], age: u64) {
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

pub(super) fn format_timestamp(timestamp: Option<u64>) -> String {
    timestamp
        .and_then(|value| DateTime::<Utc>::from_timestamp(value as i64, 0))
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into())
}

pub(super) fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    format!("{days}d {hours}h {minutes}m")
}

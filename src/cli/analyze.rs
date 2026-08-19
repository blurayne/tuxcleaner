use super::*;

pub(super) fn run_analyze(args: AnalyzeArgs) -> Result<()> {
    let home = home_dir()?;
    let root = args.path.clone().unwrap_or_else(|| home.clone());
    let minimum_size = parse_size(&args.min_size)?;
    let report = analyze(&root, minimum_size, args.max_depth)?;
    if !args.remove {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_analysis(&report);
        }
        return Ok(());
    }

    if report.root != home {
        bail!("large-file removal is limited to an analysis of the current user's home directory");
    }
    if args.yes && args.files.is_empty() {
        bail!("analyze --remove --yes requires at least one exact --file selection");
    }
    if !args.yes && (!io::stdin().is_terminal() || args.json) {
        bail!(
            "analyze --remove needs an interactive terminal; for non-interactive removal, pass exact --file paths with --yes"
        );
    }
    if !args.json {
        print_analysis(&report);
    }

    let personal: Vec<_> = report
        .large_files
        .iter()
        .filter(|file| !file.app_data)
        .cloned()
        .collect();
    let selected = if args.yes {
        select_exact_large_files(&personal, &args.files)?
    } else {
        choose_large_files(&personal)?
    };
    if selected.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"analysis": report, "results": []}))?
            );
        } else {
            println!("Nothing selected.");
        }
        return Ok(());
    }
    if !args.yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Permanently remove {} selected personal file(s)?",
                selected.len()
            ))
            .default(false)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    let distro = Distribution::detect()?;
    let executor = Executor::new(home);
    let results: Vec<_> = selected
        .iter()
        .map(|file| executor.execute(&file.cleanup_item(false), args.dry_run))
        .collect();
    record_history(&distro.name, "large-file-cleanup", &results);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"analysis": report, "results": results}))?
        );
    } else {
        print_results(&results);
    }
    fail_if_actions_failed(&results)
}

pub(super) fn select_exact_large_files(
    candidates: &[crate::analyze::LargeFile],
    requested: &[PathBuf],
) -> Result<Vec<crate::analyze::LargeFile>> {
    let mut selected = Vec::new();
    for requested_path in requested {
        let path = std::fs::canonicalize(requested_path).with_context(|| {
            format!(
                "failed to resolve selected file {}",
                requested_path.display()
            )
        })?;
        let file = candidates
            .iter()
            .find(|candidate| candidate.path == path)
            .with_context(|| {
                format!(
                    "{} is not a personal file in the current large-file analysis",
                    requested_path.display()
                )
            })?;
        if !selected
            .iter()
            .any(|existing: &crate::analyze::LargeFile| existing.path == file.path)
        {
            selected.push(file.clone());
        }
    }
    Ok(selected)
}

pub(super) fn choose_large_files(
    candidates: &[crate::analyze::LargeFile],
) -> Result<Vec<crate::analyze::LargeFile>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let labels: Vec<_> = candidates
        .iter()
        .map(|file| {
            format!(
                "{} | {} | {}",
                file.path.display(),
                format_bytes(file.size),
                format_timestamp(file.modified_unix)
            )
        })
        .collect();
    let selection = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select large personal files to permanently remove")
        .items(&labels)
        .interact()?;
    Ok(selection
        .into_iter()
        .map(|index| candidates[index].clone())
        .collect())
}

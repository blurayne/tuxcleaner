use super::*;

pub(super) fn run_clean(args: CleanArgs) -> Result<()> {
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
    record_history(&distro.name, "clean", &results);

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

pub(super) fn choose_groups(
    report: &ScanReport,
    args: &CleanArgs,
) -> Result<HashSet<CleanupGroup>> {
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

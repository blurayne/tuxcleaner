use super::*;

pub(super) fn run_uninstall(args: UninstallArgs) -> Result<()> {
    if !args.yes && !args.json && !io::stdin().is_terminal() {
        bail!(
            "uninstall needs an interactive terminal; use --app SOURCE:PACKAGE with --dry-run --yes to preview or --yes to confirm"
        );
    }
    if args.yes && args.applications.is_empty() {
        bail!("uninstall --yes requires at least one exact --app SOURCE:PACKAGE selection");
    }

    let home = home_dir()?;
    let distro = Distribution::detect()?;
    let mut report = ApplicationCatalog::system_default(home.clone(), distro.clone()).discover();
    filter_applications(&mut report, &args);

    if args.json && !args.yes {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if !args.json {
        print_application_report(&report);
    }
    if report.applications.is_empty() {
        if !args.applications.is_empty() {
            bail!(
                "none of the requested application IDs were found in the current filtered catalog; run `tuxcleaner uninstall --json` to list valid IDs"
            );
        }
        if !args.json {
            println!("No matching applications found.");
        }
        return Ok(());
    }

    let selected = if !args.applications.is_empty() {
        select_exact_applications(&report.applications, &args.applications)?
    } else {
        choose_applications(&report.applications)?
    };
    if selected.is_empty() {
        if !args.json {
            println!("Nothing selected.");
        }
        return Ok(());
    }

    let executor = Executor::new(home);
    let previews: Vec<_> = selected
        .iter()
        .map(|application| executor.preview_uninstall(application))
        .collect::<Result<_, _>>()
        .map_err(anyhow::Error::msg)?;
    if !args.json {
        print_uninstall_previews(&selected, &previews);
    }

    if !args.dry_run
        && !args.yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Uninstall {} selected application(s) while preserving user data?",
                selected.len()
            ))
            .default(false)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    let results: Vec<_> = selected
        .iter()
        .map(|application| executor.execute_uninstall(application, args.dry_run))
        .collect();
    record_history(&distro.name, "uninstall", &results);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "applications": selected,
                "previews": previews,
                "results": results,
            }))?
        );
    } else {
        print_results(&results);
        println!("Application configuration and user data were preserved.");
    }
    fail_if_actions_failed(&results)
}

pub(super) fn filter_applications(report: &mut ApplicationReport, args: &UninstallArgs) {
    if !args.source.is_empty() {
        report
            .applications
            .retain(|application| args.source.contains(&application.source));
    }
    if let Some(search) = args.search.as_deref() {
        let search = search.to_ascii_lowercase();
        report.applications.retain(|application| {
            application.name.to_ascii_lowercase().contains(&search)
                || application.package.to_ascii_lowercase().contains(&search)
                || application.id.to_ascii_lowercase().contains(&search)
        });
    }
    if !args.applications.is_empty() {
        report
            .applications
            .retain(|application| args.applications.contains(&application.id));
    }
}

pub(super) fn select_exact_applications(
    catalog: &[Application],
    requested: &[String],
) -> Result<Vec<Application>> {
    let mut selected = Vec::new();
    for id in requested {
        let application = catalog
            .iter()
            .find(|application| application.id == *id)
            .with_context(|| {
                format!(
                    "application {id} was not found in the current filtered catalog; run `tuxcleaner uninstall --json` to list valid IDs"
                )
            })?;
        if !selected
            .iter()
            .any(|existing: &Application| existing.id == application.id)
        {
            selected.push(application.clone());
        }
    }
    Ok(selected)
}

pub(super) fn choose_applications(applications: &[Application]) -> Result<Vec<Application>> {
    let labels: Vec<_> = applications
        .iter()
        .map(|application| {
            format!(
                "{:<28} {:>9}  {:<16} {}",
                truncate(&application.name, 28),
                format_bytes(application.installed_bytes),
                application.source,
                application.package
            )
        })
        .collect();
    let selection = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select applications to uninstall (nothing is selected by default)")
        .items(&labels)
        .interact()?;
    Ok(selection
        .into_iter()
        .map(|index| applications[index].clone())
        .collect())
}

pub(super) fn print_application_report(report: &ApplicationReport) {
    println!("Installed applications on {}", report.distribution);
    println!();
    println!(
        "  {:<28} {:>9}  {:<16} Package",
        "Application", "Size", "Source"
    );
    for application in &report.applications {
        println!(
            "  {:<28} {:>9}  {:<16} {}",
            truncate(&application.name, 28),
            format_bytes(application.installed_bytes),
            application.source,
            application.package
        );
        if application.user_data_bytes > 0 {
            println!(
                "    User data preserved: {} across {} path(s)",
                format_bytes(application.user_data_bytes),
                application.user_data_paths.len()
            );
        }
    }
    for warning in &report.warnings {
        println!("Warning: {warning}");
    }
}

pub(super) fn print_uninstall_previews(
    applications: &[Application],
    previews: &[crate::uninstall::UninstallPreview],
) {
    println!();
    println!("Removal plan:");
    for (application, preview) in applications.iter().zip(previews) {
        println!("  {} ({})", application.name, application.id);
        println!("    Command: {}", preview.command);
        println!("    Packages or applications affected:");
        for removal in &preview.removals {
            println!("      - {removal}");
        }
        if application.user_data_bytes > 0 {
            println!(
                "    Preserved user data: {}",
                format_bytes(application.user_data_bytes)
            );
        }
    }
}

pub(super) fn truncate(value: &str, maximum: usize) -> String {
    let mut characters = value.chars();
    let prefix: String = characters
        .by_ref()
        .take(maximum.saturating_sub(1))
        .collect();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        value.to_owned()
    }
}

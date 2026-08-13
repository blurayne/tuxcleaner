use super::*;

pub(super) fn run_purge(args: PurgeArgs) -> Result<()> {
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
    record_history(&distro.name, "purge", &results);
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

pub(super) fn choose_artifacts(candidates: &[PurgeCandidate]) -> Result<Vec<PurgeCandidate>> {
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

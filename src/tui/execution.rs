use super::*;

pub(super) enum WorkflowExecution {
    Clean {
        distribution: String,
        items: Vec<crate::model::CleanupItem>,
    },
    Uninstall {
        distribution: String,
        applications: Vec<crate::uninstall::Application>,
        previews: Vec<UninstallPreview>,
    },
    Purge {
        candidates: Vec<PurgeCandidate>,
    },
    Update,
}

impl WorkflowExecution {
    pub(super) fn needs_privilege(&self) -> bool {
        match self {
            Self::Clean { items, .. } => items
                .iter()
                .any(|item| cleanup_action_requires_root(&item.action)),
            Self::Uninstall { applications, .. } => applications
                .iter()
                .any(|application| !application.source.is_flatpak()),
            Self::Purge { .. } | Self::Update => false,
        }
    }
}

pub(super) fn cleanup_action_requires_root(action: &CleanupAction) -> bool {
    match action {
        CleanupAction::Command { requires_root, .. } => *requires_root,
        CleanupAction::CommandSequence { commands } => {
            commands.iter().any(|command| command.requires_root)
        }
        CleanupAction::RemovePath { .. } | CleanupAction::RemovePersonalFile { .. } => false,
    }
}

pub(super) fn start_workflow_load(
    action: MenuAction,
    home: PathBuf,
) -> Receiver<Result<WorkflowData, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = load_workflow(action, home).map_err(|error| format!("{error:#}"));
        let _ = sender.send(result);
    });
    receiver
}

pub(super) fn load_workflow(action: MenuAction, home: PathBuf) -> Result<WorkflowData> {
    match action {
        MenuAction::Clean => {
            let distro = Distribution::detect()?;
            Ok(WorkflowData::Clean(Scanner::new(home, distro).scan()))
        }
        MenuAction::Uninstall => {
            let distro = Distribution::detect()?;
            Ok(WorkflowData::Uninstall(
                ApplicationCatalog::system_default(home, distro).discover(),
            ))
        }
        MenuAction::Purge => {
            let roots = default_project_roots(&home)?;
            Ok(WorkflowData::Purge(scan_artifacts(&roots, 30)))
        }
        MenuAction::Status => Ok(WorkflowData::Status(status::collect()?)),
        MenuAction::History => Ok(WorkflowData::History(
            HistoryStore::system_default()?.read_recent(20)?,
        )),
        MenuAction::Update => Ok(WorkflowData::Update(update::check(None)?)),
        MenuAction::Analyze => unreachable!("Analyze has its own screen"),
    }
}

pub(super) fn start_uninstall_preview(
    home: PathBuf,
    applications: Vec<crate::uninstall::Application>,
) -> Receiver<Result<Vec<UninstallPreview>, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let executor = Executor::with_runner(home, TuiCommandRunner);
        let result = applications
            .iter()
            .map(|application| executor.preview_uninstall(application))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("{:#}", anyhow::Error::msg(error)));
        let _ = sender.send(result);
    });
    receiver
}

pub(super) fn start_workflow_execution(
    home: PathBuf,
    request: WorkflowExecution,
) -> Receiver<Result<WorkflowResult, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = execute_workflow(home, request).map_err(|error| format!("{error:#}"));
        let _ = sender.send(result);
    });
    receiver
}

pub(super) fn execute_workflow(
    home: PathBuf,
    request: WorkflowExecution,
) -> Result<WorkflowResult> {
    match request {
        WorkflowExecution::Clean {
            distribution,
            items,
        } => {
            let executor = Executor::with_runner(home, TuiCommandRunner);
            let results: Vec<_> = items
                .iter()
                .map(|item| executor.execute(item, false))
                .collect();
            record_tui_history(&distribution, "clean", &results);
            Ok(WorkflowResult::Actions(results))
        }
        WorkflowExecution::Uninstall {
            distribution,
            applications,
            previews,
        } => {
            if previews.len() != applications.len()
                || previews
                    .iter()
                    .zip(&applications)
                    .any(|(preview, application)| preview.application_id != application.id)
            {
                anyhow::bail!("the reviewed removal plan no longer matches the selection");
            }
            let executor = Executor::with_runner(home, TuiCommandRunner);
            let results: Vec<_> = applications
                .iter()
                .map(|application| executor.execute_uninstall(application, false))
                .collect();
            record_tui_history(&distribution, "uninstall", &results);
            Ok(WorkflowResult::Actions(results))
        }
        WorkflowExecution::Purge { candidates } => {
            let distribution = Distribution::detect()?.name;
            let executor = Executor::with_runner(home, TuiCommandRunner);
            let results: Vec<_> = candidates
                .iter()
                .map(PurgeCandidate::cleanup_item)
                .map(|item| executor.execute(&item, false))
                .collect();
            record_tui_history(&distribution, "purge", &results);
            Ok(WorkflowResult::Actions(results))
        }
        WorkflowExecution::Update => Ok(WorkflowResult::Update(update::install(None)?)),
    }
}

pub(super) fn record_tui_history(distribution: &str, command: &str, results: &[ActionResult]) {
    if let Ok(store) = HistoryStore::system_default() {
        let _ = store.append(&HistoryRecord {
            timestamp: Utc::now(),
            distribution: distribution.into(),
            command: command.into(),
            results: results.to_vec(),
        });
    }
}

pub(super) fn default_project_roots(home: &Path) -> Result<Vec<PathBuf>> {
    let roots: Vec<_> = ["Projects", "Progetti", "GitHub", "Code", "dev"]
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

use super::*;

#[derive(Debug, Parser)]
#[command(
    name = "tuxcleaner",
    version,
    about = "A safety-first Linux cleanup, application uninstall, and disk analysis toolkit",
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
    /// List installed desktop applications and uninstall explicit selections
    Uninstall(UninstallArgs),
    /// Analyze disk usage and optionally remove explicitly selected large files
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

#[derive(Debug, Clone, Args, Default)]
pub struct UninstallArgs {
    /// Preview removal plans without changing the system
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm exact --app selections non-interactively
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Select an application by its source-qualified ID; may be repeated
    #[arg(long = "app", value_name = "SOURCE:PACKAGE")]
    pub applications: Vec<String>,
    /// Restrict discovery to one or more application sources
    #[arg(long, value_enum, value_delimiter = ',')]
    pub source: Vec<ApplicationSource>,
    /// Filter applications by display name, package name, or ID
    #[arg(long)]
    pub search: Option<String>,
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
    /// Select large personal files for permanent removal
    #[arg(long)]
    pub remove: bool,
    /// Preview selected file removals without changing the filesystem
    #[arg(long, requires = "remove")]
    pub dry_run: bool,
    /// Confirm exact --file selections non-interactively
    #[arg(short = 'y', long, requires = "remove")]
    pub yes: bool,
    /// Select an exact large-file path from the current analysis; may be repeated
    #[arg(
        long = "file",
        value_name = "PATH",
        requires = "remove",
        requires = "yes"
    )]
    pub files: Vec<PathBuf>,
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
            remove: false,
            dry_run: false,
            yes: false,
            files: Vec::new(),
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
    /// Install a specific release, for example 0.4.0
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

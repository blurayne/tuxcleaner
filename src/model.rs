use std::fmt;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CleanupGroup {
    System,
    User,
    Dev,
    Containers,
}

impl CleanupGroup {
    pub const ALL: [Self; 4] = [Self::System, Self::User, Self::Dev, Self::Containers];

    pub fn title(self) -> &'static str {
        match self {
            Self::System => "System & packages",
            Self::User => "User & app caches",
            Self::Dev => "Developer caches",
            Self::Containers => "Docker & Flatpak",
        }
    }
}

impl fmt::Display for CleanupGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.title())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Elevated,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CleanupAction {
    RemovePath {
        path: PathBuf,
        contents_only: bool,
    },
    Command {
        program: String,
        args: Vec<String>,
        requires_root: bool,
    },
    CommandSequence {
        commands: Vec<CommandSpec>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub requires_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupItem {
    pub id: String,
    pub group: CleanupGroup,
    pub label: String,
    pub estimated_bytes: u64,
    pub risk: Risk,
    pub action: CleanupAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSummary {
    pub group: CleanupGroup,
    pub estimated_bytes: u64,
    pub item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    pub distribution: String,
    pub items: Vec<CleanupItem>,
    pub groups: Vec<GroupSummary>,
    pub estimated_total_bytes: u64,
    pub warnings: Vec<String>,
}

impl ScanReport {
    pub fn from_items(
        distribution: String,
        items: Vec<CleanupItem>,
        warnings: Vec<String>,
    ) -> Self {
        let groups = CleanupGroup::ALL
            .into_iter()
            .map(|group| {
                let matching = items.iter().filter(|item| item.group == group);
                GroupSummary {
                    group,
                    estimated_bytes: matching.clone().map(|item| item.estimated_bytes).sum(),
                    item_count: matching.count(),
                }
            })
            .collect();
        let estimated_total_bytes = items.iter().map(|item| item.estimated_bytes).sum();
        Self {
            distribution,
            items,
            groups,
            estimated_total_bytes,
            warnings,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub item_id: String,
    pub label: String,
    pub success: bool,
    pub dry_run: bool,
    pub estimated_bytes: u64,
    pub message: String,
}

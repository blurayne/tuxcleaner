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
    Models,
}

impl CleanupGroup {
    pub const ALL: [Self; 5] = [
        Self::System,
        Self::User,
        Self::Dev,
        Self::Containers,
        Self::Models,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::System => "System & packages",
            Self::User => "User & app caches",
            Self::Dev => "Developer caches",
            Self::Containers => "Docker, Podman & Flatpak",
            Self::Models => "LLM model caches",
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
    RemovePersonalFile {
        path: PathBuf,
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
        let mut groups: Vec<_> = CleanupGroup::ALL
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
        groups.sort_by(|left, right| {
            right
                .estimated_bytes
                .cmp(&left.estimated_bytes)
                .then_with(|| left.group.title().cmp(right.group.title()))
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, group: CleanupGroup, estimated_bytes: u64) -> CleanupItem {
        CleanupItem {
            id: id.into(),
            group,
            label: id.into(),
            estimated_bytes,
            risk: Risk::Low,
            action: CleanupAction::RemovePath {
                path: PathBuf::from("/tmp").join(id),
                contents_only: false,
            },
        }
    }

    #[test]
    fn from_items_orders_groups_by_estimated_bytes_descending() {
        let items = vec![
            item("system.a", CleanupGroup::System, 100),
            item("dev.a", CleanupGroup::Dev, 200),
            item("user.a", CleanupGroup::User, 50),
        ];
        let report = ScanReport::from_items("Test".into(), items, Vec::new());
        let order: Vec<_> = report.groups.iter().map(|summary| summary.group).collect();
        // Containers and Models have no items (0 bytes) and sort last, tied on
        // size and so ordered by title. System and User are both present
        // alongside Dev, which has the largest total.
        assert_eq!(
            order,
            [
                CleanupGroup::Dev,
                CleanupGroup::System,
                CleanupGroup::User,
                CleanupGroup::Containers,
                CleanupGroup::Models,
            ]
        );
    }

    #[test]
    fn from_items_breaks_group_ties_deterministically_by_title() {
        let items = vec![
            item("dev.a", CleanupGroup::Dev, 100),
            item("system.a", CleanupGroup::System, 100),
        ];
        let report = ScanReport::from_items("Test".into(), items, Vec::new());
        let tied: Vec<_> = report
            .groups
            .iter()
            .filter(|summary| summary.estimated_bytes == 100)
            .map(|summary| summary.group)
            .collect();
        // "Developer caches" sorts before "System & packages" alphabetically.
        assert_eq!(tied, [CleanupGroup::Dev, CleanupGroup::System]);
    }
}

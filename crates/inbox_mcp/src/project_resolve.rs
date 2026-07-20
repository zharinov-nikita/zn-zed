use std::path::Path;

use anyhow::{Result, anyhow};
use gpui::{App, Entity};
use inbox_panel::InboxStore;
use inbox_panel::inbox_store::InboxStoreRegistry;
use serde::Serialize;

/// A live inbox store together with the identity of the project it serves.
pub(crate) struct ResolvedProject {
    pub store: Entity<InboxStore>,
    pub worktree_root: String,
    pub project_key: String,
}

/// Snapshot of one open project for `inbox_list_projects`.
#[derive(Serialize)]
pub(crate) struct ProjectSummary {
    pub worktree_root: String,
    pub project_key: String,
    pub open_count: usize,
    pub archived_count: usize,
}

/// All live stores that are bound to a worktree, one per project. Two windows
/// on the same worktree share one KV entry; only the first registered store is
/// kept, so every mutation goes through a single in-memory copy.
pub(crate) fn live_projects(cx: &mut App) -> Vec<ResolvedProject> {
    let mut seen_keys = Vec::new();
    InboxStoreRegistry::live_stores(cx)
        .into_iter()
        .filter_map(|store| {
            let (worktree_root, project_key) = {
                let store = store.read(cx);
                (
                    store.worktree_root(cx)?.to_string_lossy().into_owned(),
                    store.bound_project_key()?.to_string(),
                )
            };
            if seen_keys.contains(&project_key) {
                return None;
            }
            seen_keys.push(project_key.clone());
            Some(ResolvedProject {
                store,
                worktree_root,
                project_key,
            })
        })
        .collect()
}

pub(crate) fn project_summaries(cx: &mut App) -> Vec<ProjectSummary> {
    live_projects(cx)
        .into_iter()
        .map(|project| {
            let store = project.store.read(cx);
            ProjectSummary {
                worktree_root: project.worktree_root,
                project_key: project.project_key,
                open_count: store.items().len(),
                archived_count: store.archived().len(),
            }
        })
        .collect()
}

/// Resolves the `project` tool parameter (an absolute worktree root path, or
/// a prefix of the project key) to a live store. With the parameter omitted,
/// succeeds only when exactly one project is open; the error message lists
/// the open projects so agents can self-correct without a schema change.
pub(crate) fn resolve_store(project: Option<&str>, cx: &mut App) -> Result<ResolvedProject> {
    let candidates = live_projects(cx);
    match project.map(str::trim).filter(|param| !param.is_empty()) {
        Some(param) => candidates
            .into_iter()
            .find(|candidate| {
                same_path(param, &candidate.worktree_root)
                    || candidate.project_key.starts_with(param)
            })
            .ok_or_else(|| {
                anyhow!(
                    "no open project matches {param:?}; open projects with an inbox: {}",
                    project_list_for_error(cx)
                )
            }),
        None => match candidates.len() {
            1 => Ok(candidates.into_iter().next().unwrap()),
            0 => Err(anyhow!(
                "no project with an inbox is open in Zed (the inbox panel binds to the first \
                 visible worktree of each window)"
            )),
            _ => Err(anyhow!(
                "several projects are open; pass `project` (worktree root path or project key): {}",
                candidates
                    .iter()
                    .map(|candidate| {
                        format!("{} ({})", candidate.worktree_root, candidate.project_key)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

fn project_list_for_error(cx: &mut App) -> String {
    let projects = live_projects(cx);
    if projects.is_empty() {
        return "none".to_string();
    }
    projects
        .iter()
        .map(|project| format!("{} ({})", project.worktree_root, project.project_key))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Path equality via component iteration, so `/` and `\` spellings of the
/// same Windows path compare equal.
fn same_path(a: &str, b: &str) -> bool {
    Path::new(a).components().eq(Path::new(b).components())
}

use std::collections::{BTreeMap, HashSet};

use git::{ToggleDiffTreePanel, repository::RepoPath, status::FileStatus};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, Styled, WeakEntity, Window, div, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::branch_diff::BranchDiff;

pub struct DiffTreePanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    collapsed_dirs: HashSet<String>,
}

struct TreeDir {
    dirs: BTreeMap<String, TreeDir>,
    files: Vec<(String, RepoPath, FileStatus)>,
}

impl TreeDir {
    fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: Vec::new(),
        }
    }

    fn insert(&mut self, repo_path: &RepoPath, status: FileStatus) {
        let components: Vec<&str> = repo_path.as_unix_str().split('/').collect();
        let mut current = self;
        for component in &components[..components.len().saturating_sub(1)] {
            current = current
                .dirs
                .entry((*component).to_string())
                .or_insert_with(TreeDir::new);
        }
        if let Some(name) = components.last() {
            current
                .files
                .push(((*name).to_string(), repo_path.clone(), status));
        }
    }

    fn file_count(&self) -> usize {
        self.files.len() + self.dirs.values().map(TreeDir::file_count).sum::<usize>()
    }
}

fn status_badge(status: FileStatus) -> (&'static str, gpui::Hsla) {
    match status {
        FileStatus::Untracked => ("A", gpui::hsla(0.33, 0.8, 0.45, 1.0)),
        FileStatus::Ignored => ("!", gpui::hsla(0.0, 0.0, 0.6, 1.0)),
        FileStatus::Unmerged { .. } => ("U", gpui::hsla(0.08, 0.9, 0.5, 1.0)),
        FileStatus::Tracked(tracked) => {
            let tracked_changed_added = tracked.index_status == git::status::StatusCode::Added
                || tracked.worktree_status == git::status::StatusCode::Added;
            let tracked_changed_deleted = tracked.index_status == git::status::StatusCode::Deleted
                || tracked.worktree_status == git::status::StatusCode::Deleted;
            let tracked_changed_renamed = tracked.index_status == git::status::StatusCode::Renamed
                || tracked.worktree_status == git::status::StatusCode::Renamed;
            if tracked_changed_added {
                ("A", gpui::hsla(0.33, 0.8, 0.45, 1.0))
            } else if tracked_changed_deleted {
                ("D", gpui::hsla(0.0, 0.8, 0.5, 1.0))
            } else if tracked_changed_renamed {
                ("R", gpui::hsla(0.58, 0.8, 0.55, 1.0))
            } else {
                ("M", gpui::hsla(0.11, 0.9, 0.5, 1.0))
            }
        }
    }
}

impl DiffTreePanel {
    pub(crate) fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            collapsed_dirs: HashSet::default(),
        }
    }

    fn active_branch_diff(&self, cx: &App) -> Option<Entity<BranchDiff>> {
        let workspace = self.workspace.upgrade()?;
        workspace.read(cx).items_of_type::<BranchDiff>(cx).last()
    }

    fn render_rows(
        &self,
        dir: &TreeDir,
        parent_path: String,
        depth: usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        for (name, sub_dir) in &dir.dirs {
            let path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            let collapsed = self.collapsed_dirs.contains(&path);
            let chevron = if collapsed { "\u{25B8}" } else { "\u{25BE}" };
            let count = sub_dir.file_count();
            let toggle_path = path.clone();
            rows.push(
                h_flex()
                    .id(ElementId::Name(format!("panel-dir-{path}").into()))
                    .h(px(22.))
                    .pl(px(6. + depth as f32 * 12.))
                    .pr_2()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        if !this.collapsed_dirs.remove(&toggle_path) {
                            this.collapsed_dirs.insert(toggle_path.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        Label::new(chevron)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Icon::new(IconName::Folder).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(name.clone()).size(LabelSize::Small))
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{count}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
            if !collapsed {
                self.render_rows(sub_dir, path, depth + 1, rows, cx);
            }
        }
        for (name, repo_path, status) in &dir.files {
            let target = repo_path.clone();
            let (status_label, status_color) = status_badge(*status);
            rows.push(
                h_flex()
                    .id(ElementId::Name(
                        format!("panel-file-{}", repo_path.as_unix_str()).into(),
                    ))
                    .h(px(22.))
                    .pl(px(6. + depth as f32 * 12.))
                    .pr_2()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(branch_diff) = this.active_branch_diff(cx) {
                            branch_diff.update(cx, |diff, cx| {
                                diff.move_to_repo_path(&target, window, cx)
                            });
                        }
                    }))
                    .child(
                        Label::new(status_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Custom(status_color)),
                    )
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(name.clone()).size(LabelSize::Small))
                    .into_any_element(),
            );
        }
    }
}

impl Focusable for DiffTreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DiffTreePanel {}

impl Render for DiffTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self
            .active_branch_diff(cx)
            .map(|diff| diff.read(cx).changed_files(cx))
            .unwrap_or_default();

        let mut root = TreeDir::new();
        for (repo_path, status) in &entries {
            root.insert(repo_path, *status);
        }
        let mut rows = Vec::new();
        self.render_rows(&root, String::new(), 0, &mut rows, cx);

        let colors = cx.theme().colors();
        v_flex()
            .id("diff-tree-panel")
            .size_full()
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new("Changed Files").size(LabelSize::Small)),
            )
            .child(
                div()
                    .id("diff-tree-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .py_1()
                    .when(entries.is_empty(), |this| {
                        this.child(
                            Label::new("Open a branch comparison to see changed files")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(rows),
            )
    }
}

impl Panel for DiffTreePanel {
    fn persistent_name() -> &'static str {
        "DiffTreePanel"
    }

    fn panel_key() -> &'static str {
        "DiffTreePanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Changed Files")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleDiffTreePanel)
    }

    fn activation_priority(&self) -> u32 {
        0
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
}

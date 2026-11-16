use std::path::PathBuf;

use anyhow::Context as _;
use git::{
    ResolveConflicts,
    repository::RepoPath,
    status::{FileStatus, UnmergedStatusCode},
};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div,
};
use gpui::Subscription;
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ModalView, Workspace,
    notifications::DetachAndPromptErr,
};

struct ConflictEntry {
    repo_path: RepoPath,
    display_name: SharedString,
    display_dir: SharedString,
    ours_status: &'static str,
    theirs_status: &'static str,
    abs_path: PathBuf,
}

pub struct ConflictResolutionModal {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo: Entity<Repository>,
    entries: Vec<ConflictEntry>,
    selected: Option<usize>,
    header: SharedString,
    ours_label: SharedString,
    theirs_label: SharedString,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl ConflictResolutionModal {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &ResolveConflicts, window, cx| {
            Self::open(workspace, window, cx);
        });
    }

    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let project = workspace.project().clone();
        let Some(repo) = project.read(cx).active_repository(cx) else {
            return;
        };
        let has_conflicts = repo
            .read(cx)
            .cached_status()
            .any(|entry| entry.status.is_conflicted());
        if !has_conflicts {
            return;
        }
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(workspace_handle, project, repo, window, cx)
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        repo: Entity<Repository>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (header, ours_label, theirs_label) = {
            let snapshot = repo.read(cx).snapshot();
            let ours: SharedString = snapshot
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
                .unwrap_or_else(|| SharedString::from("ours"));
            let theirs: SharedString = snapshot
                .merge
                .merge_heads_by_conflicted_path
                .values()
                .find_map(|heads| heads.iter().find_map(|head| head.clone()))
                .map(|sha| SharedString::from(sha.chars().take(7).collect::<String>()))
                .unwrap_or_else(|| SharedString::from("theirs"));
            let header = SharedString::from(format!(
                "Merge branch {} into {}",
                theirs, ours
            ));
            (header, ours, theirs)
        };

        let focus_handle = cx.focus_handle();
        let subscription = cx.subscribe(&repo, |this, _repo, event: &RepositoryEvent, cx| {
            if matches!(event, RepositoryEvent::StatusesChanged) {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            workspace,
            project,
            repo,
            entries: Vec::new(),
            selected: None,
            header,
            ours_label,
            theirs_label,
            focus_handle,
            _subscription: subscription,
        };
        this.refresh(cx);
        if !this.entries.is_empty() {
            this.selected = Some(0);
        }
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let mut entries = Vec::new();
        for status_entry in self.repo.read(cx).cached_status() {
            let FileStatus::Unmerged(unmerged) = status_entry.status else {
                continue;
            };
            let repo_path = status_entry.repo_path.clone();
            let Some(project_path) = self
                .repo
                .read(cx)
                .repo_path_to_project_path(&repo_path, cx)
            else {
                continue;
            };
            let Some(abs_path) = self.project.read(cx).absolute_path(&project_path, cx) else {
                continue;
            };
            let display_path = repo_path.as_unix_str().to_string();
            let (display_name, display_dir) = match display_path.rsplit_once('/') {
                Some((dir, name)) => (SharedString::from(name.to_string()), SharedString::from(dir.to_string())),
                None => (SharedString::from(display_path.clone()), SharedString::from("")),
            };
            entries.push(ConflictEntry {
                repo_path,
                display_name,
                display_dir,
                ours_status: status_label(unmerged.first_head),
                theirs_status: status_label(unmerged.second_head),
                abs_path,
            });
        }
        entries.sort_by(|left, right| left.display_name.cmp(&right.display_name));
        self.entries = entries;
        if self.selected.is_some_and(|index| index >= self.entries.len()) {
            self.selected = None;
        }
        if self.entries.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn accept_side(&mut self, take_ours: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.and_then(|index| self.entries.get(index)) else {
            return;
        };
        let repo_path = entry.repo_path.clone();
        let abs_path = entry.abs_path.clone();
        let repo = self.repo.clone();
        let stage_texts = repo.read(cx).load_merge_stage_texts(repo_path.clone(), cx);

        cx.spawn_in(window, async move |this, cx| {
            let (_base, ours, theirs) = stage_texts.await?;
            let chosen = if take_ours { ours } else { theirs };
            match chosen {
                Some(content) => {
                    std::fs::write(&abs_path, content).context("failed to write resolved file")?;
                }
                None => {
                    // The chosen side deleted the file, so delete it in the worktree.
                    if abs_path.exists() {
                        std::fs::remove_file(&abs_path)
                            .context("failed to remove file for deleted side")?;
                    }
                }
            }

            repo.update(cx, |repo, cx| repo.stage_entries(vec![repo_path.clone()], cx))
                .await?;

            this.update_in(cx, |this, _window, cx| {
                this.refresh(cx);
                if !this.entries.is_empty() {
                    this.selected = Some(0);
                }
            })
            .log_err();

            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to accept side", window, cx, |error, _, _| {
            Some(error.to_string())
        });
    }

    fn open_merge_editor(
        &mut self,
        repo_path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(
            Box::new(git::OpenMergeEditor {
                path: repo_path.as_unix_str().to_string(),
            }),
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn render_row(&self, index: usize, entry: &ConflictEntry, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected == Some(index);
        let selected_bg = cx.theme().colors().element_selected;
        let hover_bg = cx.theme().colors().element_hover;
        h_flex()
            .id(("conflict-row", index))
            .w_full()
            .h(px(34.))
            .px_2()
            .gap_2()
            .items_center()
            .when(selected, |this| this.bg(selected_bg))
            .hover(|this| this.bg(hover_bg))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.selected = Some(index);
                cx.notify();
            }))
            .child(Icon::new(IconName::File).size(IconSize::XSmall))
            .child(
                Label::new(entry.display_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .child(
                Label::new(entry.display_dir.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                div().w(px(140.)).child(
                    Label::new(entry.ours_status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().w(px(140.)).child(
                    Label::new(entry.theirs_status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for ConflictResolutionModal {}
impl ModalView for ConflictResolutionModal {}

impl Focusable for ConflictResolutionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConflictResolutionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.entries.len();
        let selected_repo_path = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.repo_path.clone());
        let merge_path = selected_repo_path.clone();
        let border_variant = cx.theme().colors().border_variant;

        let rows = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| self.render_row(index, entry, cx))
            .collect::<Vec<_>>();

        v_flex()
            .key_context("ConflictResolutionModal")
            .on_action(cx.listener(Self::cancel))
            .elevation_2(cx)
            .w(rems(60.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(Headline::new(format!("Conflicts ({count})")).size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .w_full()
                    .child(Label::new(self.header.clone()).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_y_1()
                    .border_color(border_variant)
                    .child(div().flex_1().child(
                        Label::new("Name")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ))
                    .child(
                        div().w(px(140.)).child(
                            Label::new(format!("Your changes ({})", self.ours_label))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        div().w(px(140.)).child(
                            Label::new(format!("Their changes ({})", self.theirs_label))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_h_0()
                    .child(
                        div()
                            .id("conflict-list")
                            .flex_1()
                            .max_h(rems(24.))
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .child(
                        v_flex()
                            .w(rems(14.))
                            .p_2()
                            .gap_2()
                            .child(
                                Button::new("accept-ours", "Accept Yours")
                                    .label_size(LabelSize::Small)
                                    .disabled(selected_repo_path.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.accept_side(true, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("accept-theirs", "Accept Theirs")
                                    .label_size(LabelSize::Small)
                                    .disabled(selected_repo_path.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.accept_side(false, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("merge-file", "Merge...")
                                    .label_size(LabelSize::Small)
                                    .disabled(merge_path.is_none())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(path) = merge_path.clone() {
                                            this.open_merge_editor(path, window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(border_variant)
                    .child(div().flex_1())
                    .child(
                        Button::new("close-conflicts", "Close")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|_, _, _window, cx| {
                                cx.emit(DismissEvent);
                            })),
                    ),
            )
    }
}

fn status_label(code: UnmergedStatusCode) -> &'static str {
    match code {
        UnmergedStatusCode::Updated => "Modified",
        UnmergedStatusCode::Deleted => "Deleted",
        UnmergedStatusCode::Added => "Added",
    }
}

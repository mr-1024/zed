use std::sync::Arc;

use anyhow::Context as _;
use editor::{
    Editor, RowHighlightOptions,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription, WeakEntity, Window,
    div,
};
use git::{OpenMergeEditor, ResolveConflicts, repository::RepoPath};
use language::Buffer;
use multi_buffer::MultiBufferOffset;
use project::{Project, git_store::Repository};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Item, Workspace,
    item::ItemEvent,
    notifications::{DetachAndPromptErr, NotifyTaskExt},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FragmentKind {
    Equal,
    OursOnly,
    TheirsOnly,
    BothSame,
    Conflict,
}

#[derive(Clone)]
struct Fragment {
    kind: FragmentKind,
    ours: (usize, usize),
    theirs: (usize, usize),
    base: (usize, usize),
    result_start: usize,
    result_end: usize,
    ours_arrow: Option<CustomBlockId>,
    theirs_arrow: Option<CustomBlockId>,
    resolved: bool,
}

pub struct MergeView {
    project: Entity<Project>,
    focus_handle: FocusHandle,
    file_name: SharedString,
    left_label: SharedString,
    right_label: SharedString,
    ours_lines: Vec<String>,
    theirs_lines: Vec<String>,
    base_lines: Vec<String>,
    repo: Entity<Repository>,
    repo_path: RepoPath,
    result_buffer: Entity<Buffer>,
    result_editor: Entity<Editor>,
    ours_editor: Entity<Editor>,
    base_editor: Entity<Editor>,
    theirs_editor: Entity<Editor>,
    show_base: bool,
    fragments: Vec<Fragment>,
    initial_fragments: Vec<Fragment>,
    initial_result_text: String,
    _subscriptions: Vec<Subscription>,
}

impl MergeView {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, action: &OpenMergeEditor, window, cx| {
            Self::open(workspace, action, window, cx);
        });
    }

    fn open(
        workspace: &mut Workspace,
        action: &OpenMergeEditor,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            return;
        };
        let Ok(repo_path) = RepoPath::new(&action.path) else {
            return;
        };
        let Some(project_path) = repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
        else {
            return;
        };

        let (left_label, right_label) = {
            let repo = repository.read(cx);
            let snapshot = repo.snapshot();
            let left: SharedString = snapshot
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
                .unwrap_or_else(|| SharedString::from("Ours"));
            let right: SharedString = snapshot
                .merge
                .merge_heads_by_conflicted_path
                .get(&repo_path)
                .and_then(|heads| heads.iter().find_map(|head| head.clone()))
                .map(|sha| SharedString::from(sha.chars().take(7).collect::<String>()))
                .unwrap_or_else(|| SharedString::from("Theirs"));
            (left, right)
        };

        let stage_texts = repository
            .read(cx)
            .load_merge_stage_texts(repo_path.clone(), cx);
        let workspace_handle = cx.entity().downgrade();

        window
            .spawn(cx, async move |cx| {
                let (base, ours, theirs) = stage_texts.await?;
                let base = base.unwrap_or_default();
                let ours = ours.unwrap_or_default();
                let theirs = theirs.unwrap_or_default();

                let base_lines = split_lines(&base);
                let ours_lines = split_lines(&ours);
                let theirs_lines = split_lines(&theirs);
                let mut fragments = compute_fragments(&base_lines, &ours_lines, &theirs_lines);
                let result_text =
                    build_result(&mut fragments, &base_lines, &ours_lines, &theirs_lines);

                let file_buffer = project
                    .update(cx, |project, cx| {
                        project.open_buffer(project_path.clone(), cx)
                    })
                    .await?;
                file_buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer
                        .edit([(0..len, result_text.clone())], None, cx)
                        .context("failed to write merged text")
                })?;

                let file_name: SharedString = project_path
                    .path
                    .file_name()
                    .map(|name| SharedString::from(name.to_string()))
                    .unwrap_or_else(|| SharedString::from("conflicted file"));

                workspace_handle
                    .update_in(cx, |workspace, window, cx| {
                        let view = cx.new(|cx| {
                            MergeView::new(
                                project.clone(),
                                repository.clone(),
                                repo_path.clone(),
                                file_buffer,
                                base_lines,
                                ours_lines,
                                theirs_lines,
                                fragments,
                                result_text.clone(),
                                file_name,
                                left_label,
                                right_label,
                                window,
                                cx,
                            )
                        });
                        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                    })
                    .log_err();

                anyhow::Ok(())
            })
            .detach_and_prompt_err("Failed to open merge editor", window, cx, |error, _, _| {
                Some(error.to_string())
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        project: Entity<Project>,
        repo: Entity<Repository>,
        repo_path: RepoPath,
        file_buffer: Entity<Buffer>,
        base_lines: Vec<String>,
        ours_lines: Vec<String>,
        theirs_lines: Vec<String>,
        mut fragments: Vec<Fragment>,
        result_text: String,
        file_name: SharedString,
        left_label: SharedString,
        right_label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base_text = base_lines.concat();
        let ours_text = ours_lines.concat();
        let theirs_text = theirs_lines.concat();

        let make_read_only_editor = |text: &str, window: &mut Window, cx: &mut Context<Self>| {
            let buffer = cx.new(|cx| Buffer::local(text, cx));
            let editor =
                cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
            editor.update(cx, |editor, _| editor.set_read_only(true));
            editor
        };

        let ours_editor = make_read_only_editor(&ours_text, window, cx);
        let base_editor = make_read_only_editor(&base_text, window, cx);
        let theirs_editor = make_read_only_editor(&theirs_text, window, cx);
        let result_editor =
            cx.new(|cx| Editor::for_buffer(file_buffer.clone(), Some(project.clone()), window, cx));

        let ours_ranges = highlight_ranges(
            &fragments,
            &ours_lines,
            Side::Ours,
            |kind| {
                matches!(
                    kind,
                    FragmentKind::OursOnly | FragmentKind::BothSame | FragmentKind::Conflict
                )
            },
        );
        let theirs_ranges = highlight_ranges(
            &fragments,
            &theirs_lines,
            Side::Theirs,
            |kind| {
                matches!(
                    kind,
                    FragmentKind::TheirsOnly | FragmentKind::BothSame | FragmentKind::Conflict
                )
            },
        );
        highlight_changed_rows(
            &ours_editor,
            ours_ranges,
            |cx| cx.theme().colors().version_control_conflict_marker_ours,
            cx,
        );
        highlight_changed_rows(
            &theirs_editor,
            theirs_ranges,
            |cx| cx.theme().colors().version_control_conflict_marker_theirs,
            cx,
        );

        let view = cx.entity().downgrade();
        let ours_offsets = line_offsets(&ours_text);
        let theirs_offsets = line_offsets(&theirs_text);
        let ours_arrows = insert_side_arrows(
            &ours_editor,
            &fragments,
            &ours_offsets,
            Side::Ours,
            view.clone(),
            cx,
        );
        for (index, block_id) in ours_arrows {
            if let Some(fragment) = fragments.get_mut(index) {
                fragment.ours_arrow = Some(block_id);
            }
        }
        let theirs_arrows = insert_side_arrows(
            &theirs_editor,
            &fragments,
            &theirs_offsets,
            Side::Theirs,
            view,
            cx,
        );
        for (index, block_id) in theirs_arrows {
            if let Some(fragment) = fragments.get_mut(index) {
                fragment.theirs_arrow = Some(block_id);
            }
        }

        let focus_handle = result_editor.focus_handle(cx);

        Self {
            project,
            focus_handle,
            file_name,
            left_label,
            right_label,
            ours_lines,
            theirs_lines,
            base_lines,
            repo,
            repo_path,
            result_buffer: file_buffer,
            result_editor,
            ours_editor,
            base_editor,
            theirs_editor,
            show_base: false,
            initial_fragments: fragments.clone(),
            initial_result_text: result_text,
            fragments,
            _subscriptions: Vec::new(),
        }
    }

    fn accept_fragment(&mut self, index: usize, side: Side, cx: &mut Context<Self>) {
        let Some(fragment) = self.fragments.get(index) else {
            return;
        };
        if fragment.kind == FragmentKind::Equal {
            return;
        }
        let (start, end) = (fragment.result_start, fragment.result_end);
        let text = match side {
            Side::Ours => lines_text(&self.ours_lines, fragment.ours),
            Side::Theirs => lines_text(&self.theirs_lines, fragment.theirs),
        };
        let delta = text.len() as isize - (end.saturating_sub(start)) as isize;

        if let Some(block_id) = fragment.ours_arrow {
            self.ours_editor.update(cx, |editor, cx| {
                editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
            });
        }
        if let Some(block_id) = fragment.theirs_arrow {
            self.theirs_editor.update(cx, |editor, cx| {
                editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
            });
        }

        self.result_buffer.update(cx, |buffer, cx| {
            buffer.edit([(start..end, text.clone())], None, cx);
        });

        let fragment = &mut self.fragments[index];
        fragment.result_end = fragment.result_start + text.len();
        fragment.resolved = true;
        fragment.ours_arrow = None;
        fragment.theirs_arrow = None;
        for later in self.fragments.iter_mut().skip(index + 1) {
            later.result_start = (later.result_start as isize + delta).max(0) as usize;
            later.result_end = (later.result_end as isize + delta).max(0) as usize;
        }
        cx.notify();
    }

    fn ignore_fragment(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(fragment) = self.fragments.get_mut(index) else {
            return;
        };
        if fragment.kind == FragmentKind::Equal {
            return;
        }
        if let Some(block_id) = fragment.ours_arrow.take() {
            self.ours_editor.update(cx, |editor, cx| {
                editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
            });
        }
        if let Some(block_id) = fragment.theirs_arrow.take() {
            self.theirs_editor.update(cx, |editor, cx| {
                editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
            });
        }
        let fragment = self.fragments.get_mut(index).unwrap();
        fragment.resolved = true;
        cx.notify();
    }

    fn accept_side(&mut self, take_ours: bool, cx: &mut Context<Self>) {
        let text = if take_ours {
            self.ours_lines.concat()
        } else {
            self.theirs_lines.concat()
        };
        self.result_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        cx.notify();
    }

    fn accept_both(&mut self, cx: &mut Context<Self>) {
        for fragment in &self.fragments {
            if let Some(block_id) = fragment.ours_arrow {
                self.ours_editor.update(cx, |editor, cx| {
                    editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
                });
            }
            if let Some(block_id) = fragment.theirs_arrow {
                self.theirs_editor.update(cx, |editor, cx| {
                    editor.remove_blocks(std::iter::once(block_id).collect(), None, cx);
                });
            }
        }

        let text = self.initial_result_text.clone();
        self.result_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });

        self.fragments = self.initial_fragments.clone();

        let ours_offsets = line_offsets(&self.ours_lines.concat());
        let theirs_offsets = line_offsets(&self.theirs_lines.concat());
        let view = cx.entity().downgrade();
        let ours_arrows = insert_side_arrows(
            &self.ours_editor,
            &self.fragments,
            &ours_offsets,
            Side::Ours,
            view.clone(),
            cx,
        );
        for (index, block_id) in ours_arrows {
            if let Some(fragment) = self.fragments.get_mut(index) {
                fragment.ours_arrow = Some(block_id);
            }
        }
        let theirs_arrows = insert_side_arrows(
            &self.theirs_editor,
            &self.fragments,
            &theirs_offsets,
            Side::Theirs,
            view,
            cx,
        );
        for (index, block_id) in theirs_arrows {
            if let Some(fragment) = self.fragments.get_mut(index) {
                fragment.theirs_arrow = Some(block_id);
            }
        }
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let repo = self.repo.clone();
        let repo_path = self.repo_path.clone();
        let buffer = self.result_buffer.clone();

        cx.spawn_in(window, async move |this, cx| {
            project
                .update(cx, |project, cx| project.save_buffer(buffer, cx))
                .await?;
            repo.update(cx, |repo, cx| repo.stage_entries(vec![repo_path], cx))
                .await?;
            this.update_in(cx, |_this, window, cx| {
                window.dispatch_action(Box::new(ResolveConflicts), cx);
                cx.emit(ItemEvent::CloseItem);
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to apply merge", window, cx, |error, _, _| {
            Some(error.to_string())
        });
    }

    fn pane(
        &self,
        label: &'static str,
        detail: Option<SharedString>,
        editor: Entity<Editor>,
        cx: &App,
    ) -> AnyElement {
        let mut header = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(label).size(LabelSize::Small));
        if let Some(detail) = detail {
            header = header.child(
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .child(header)
            .child(div().flex_1().min_h_0().child(editor))
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum Side {
    Ours,
    Theirs,
}

impl Focusable for MergeView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for MergeView {}

impl Item for MergeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Merge {}", self.file_name).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Merge Editor Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event);
    }
}

impl Render for MergeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let unresolved = self
            .fragments
            .iter()
            .filter(|fragment| fragment.kind == FragmentKind::Conflict && !fragment.resolved)
            .count();
        let changes = self
            .fragments
            .iter()
            .filter(|fragment| fragment.kind != FragmentKind::Equal)
            .count();
        v_flex()
            .size_full()
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(format!("Merge: {}", self.file_name)).size(LabelSize::Small),
                    )
                    .child(
                        Label::new(format!("{changes} change(s), {unresolved} conflict(s)"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("header-accept-left", "Accept Left")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.accept_side(true, cx);
                                    })),
                            )
                            .child(
                                Button::new("header-accept-both", "Accept Both")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.accept_both(cx);
                                    })),
                            )
                            .child(
                                Button::new("header-accept-right", "Accept Right")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.accept_side(false, cx);
                                    })),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("toggle-base", "Base")
                            .label_size(LabelSize::Small)
                            .toggle_state(self.show_base)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.show_base = !this.show_base;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.pane(
                        "Ours",
                        Some(self.left_label.clone()),
                        self.ours_editor.clone(),
                        cx,
                    ))
                    .when(self.show_base, |this| {
                        this.child(self.pane("Base", None, self.base_editor.clone(), cx))
                    })
                    .child(self.pane("Result", None, self.result_editor.clone(), cx))
                    .child(self.pane(
                        "Theirs",
                        Some(self.right_label.clone()),
                        self.theirs_editor.clone(),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-merge", "Cancel")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(Box::new(ResolveConflicts), cx);
                                cx.emit(ItemEvent::CloseItem);
                            })),
                    )
                    .child(
                        Button::new("apply-merge", "Apply")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply(window, cx);
                            })),
                    ),
            )
    }
}

fn split_lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .map(|line| line.to_string())
        .collect()
}

fn normalized(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offsets.push(offset);
        offset += line.len();
    }
    offsets.push(offset);
    offsets
}

fn lines_text(lines: &[String], (start, len): (usize, usize)) -> String {
    lines
        .iter()
        .skip(start)
        .take(len)
        .map(|line| line.as_str())
        .collect()
}

fn unchanged_ranges(base: &[String], side: &[String]) -> Vec<(usize, usize, usize, usize)> {
    let base_text = normalized(base);
    let side_text = normalized(side);
    let diff = similar::TextDiff::from_lines(&base_text, &side_text);
    let mut ranges = Vec::new();
    for op in diff.ops() {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            ranges.push((
                *old_index,
                old_index + len,
                *new_index,
                new_index + len,
            ));
        }
    }
    ranges
}

/// Port of IntelliJ's `ComparisonMergeUtil.FairMergeBuilder`: merge fragments are
/// delimited by the equal ranges shared by both side-diffs. This keeps adjacent
/// insert/delete edits on opposite sides as separate, non-conflicting fragments.
fn compute_fragments(base: &[String], ours: &[String], theirs: &[String]) -> Vec<Fragment> {
    let ours_unchanged = unchanged_ranges(base, ours);
    let theirs_unchanged = unchanged_ranges(base, theirs);

    let mut changes: Vec<(usize, usize, usize, usize, usize, usize)> = Vec::new();
    let (mut idx_o, mut idx_b, mut idx_t) = (0usize, 0usize, 0usize);
    let (mut i, mut j) = (0usize, 0usize);

    while i < ours_unchanged.len() && j < theirs_unchanged.len() {
        let r1 = ours_unchanged[i];
        let r2 = theirs_unchanged[j];

        if r1.1 <= r2.0 {
            i += 1;
            continue;
        }
        if r2.1 <= r1.0 {
            j += 1;
            continue;
        }

        let start_base = r1.0.max(r2.0);
        let end_base = r1.1.min(r2.1);
        let count = end_base - start_base;
        let start_ours = r1.2 + (start_base - r1.0);
        let end_ours = start_ours + count;
        let start_theirs = r2.2 + (start_base - r2.0);
        let end_theirs = start_theirs + count;

        if idx_o != start_ours || idx_b != start_base || idx_t != start_theirs {
            changes.push((idx_o, start_ours, idx_b, start_base, idx_t, start_theirs));
        }
        idx_o = end_ours;
        idx_b = end_base;
        idx_t = end_theirs;

        if r1.1 <= r2.1 {
            i += 1;
        } else {
            j += 1;
        }
    }

    if idx_o != ours.len() || idx_b != base.len() || idx_t != theirs.len() {
        changes.push((idx_o, ours.len(), idx_b, base.len(), idx_t, theirs.len()));
    }

    let mut fragments = Vec::new();
    let (mut prev_o, mut prev_b, mut prev_t) = (0usize, 0usize, 0usize);
    for (o_s, o_e, b_s, b_e, t_s, t_e) in changes {
        if o_s > prev_o || b_s > prev_b || t_s > prev_t {
            fragments.push(Fragment {
                kind: FragmentKind::Equal,
                ours: (prev_o, o_s - prev_o),
                base: (prev_b, b_s - prev_b),
                theirs: (prev_t, t_s - prev_t),
                result_start: 0,
                result_end: 0,
                ours_arrow: None,
                theirs_arrow: None,
                resolved: false,
            });
        }

        let ours_slice = &ours[o_s..o_e];
        let base_slice = &base[b_s..b_e];
        let theirs_slice = &theirs[t_s..t_e];
        let ours_norm = normalized(ours_slice);
        let theirs_norm = normalized(theirs_slice);
        let base_norm = normalized(base_slice);
        let ours_changed = ours_norm != base_norm;
        let theirs_changed = theirs_norm != base_norm;
        let kind = match (ours_changed, theirs_changed) {
            (false, false) => FragmentKind::Equal,
            (true, false) => FragmentKind::OursOnly,
            (false, true) => FragmentKind::TheirsOnly,
            (true, true) => {
                if ours_norm == theirs_norm {
                    FragmentKind::BothSame
                } else {
                    FragmentKind::Conflict
                }
            }
        };

        fragments.push(Fragment {
            kind,
            ours: (o_s, o_e - o_s),
            base: (b_s, b_e - b_s),
            theirs: (t_s, t_e - t_s),
            result_start: 0,
            result_end: 0,
            ours_arrow: None,
            theirs_arrow: None,
            resolved: false,
        });

        prev_o = o_e;
        prev_b = b_e;
        prev_t = t_e;
    }

    if prev_o < ours.len() || prev_b < base.len() || prev_t < theirs.len() {
        fragments.push(Fragment {
            kind: FragmentKind::Equal,
            ours: (prev_o, ours.len() - prev_o),
            base: (prev_b, base.len() - prev_b),
            theirs: (prev_t, theirs.len() - prev_t),
            result_start: 0,
            result_end: 0,
            ours_arrow: None,
            theirs_arrow: None,
            resolved: false,
        });
    }

    fragments
}

fn build_result(
    fragments: &mut [Fragment],
    base: &[String],
    ours: &[String],
    theirs: &[String],
) -> String {
    let mut result = String::new();
    for fragment in fragments.iter_mut() {
        let text = match fragment.kind {
            FragmentKind::Equal => {
                let ours_text = lines_text(ours, fragment.ours);
                let base_text = lines_text(base, fragment.base);
                let theirs_text = lines_text(theirs, fragment.theirs);
                if ours_text != base_text {
                    ours_text
                } else if theirs_text != base_text {
                    theirs_text
                } else {
                    base_text
                }
            }
            FragmentKind::OursOnly | FragmentKind::BothSame => lines_text(ours, fragment.ours),
            FragmentKind::TheirsOnly => lines_text(theirs, fragment.theirs),
            FragmentKind::Conflict => {
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push('\n');
                }
                "\n".to_string()
            }
        };
        let start = result.len();
        result.push_str(&text);
        fragment.result_start = start;
        fragment.result_end = result.len();
    }
    result
}

fn highlight_ranges(
    fragments: &[Fragment],
    lines: &[String],
    side: Side,
    predicate: impl Fn(FragmentKind) -> bool,
) -> Vec<(usize, usize)> {
    let text: String = lines.concat();
    let offsets = line_offsets(&text);
    let total = text.len();
    fragments
        .iter()
        .filter(|fragment| fragment.kind != FragmentKind::Equal && predicate(fragment.kind))
        .filter_map(|fragment| {
            let (start_line, len) = match side {
                Side::Ours => fragment.ours,
                Side::Theirs => fragment.theirs,
            };
            if len == 0 {
                return None;
            }
            let start = offsets.get(start_line).copied().unwrap_or(total);
            let end = offsets
                .get(start_line + len)
                .copied()
                .unwrap_or(total);
            Some((start, end))
        })
        .collect()
}

fn insert_side_arrows(
    editor: &Entity<Editor>,
    fragments: &[Fragment],
    offsets: &[usize],
    side: Side,
    view: WeakEntity<MergeView>,
    cx: &mut App,
) -> Vec<(usize, CustomBlockId)> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut indices = Vec::new();
        let blocks = fragments
            .iter()
            .enumerate()
            .filter_map(|(index, fragment)| {
                if fragment.kind == FragmentKind::Equal {
                    return None;
                }
                let (start_line, len) = match side {
                    Side::Ours => fragment.ours,
                    Side::Theirs => fragment.theirs,
                };
                if len == 0 {
                    return None;
                }
                let offset = offsets.get(start_line).copied()?;
                indices.push(index);
                let anchor = snapshot.anchor_before(MultiBufferOffset(offset));
                let view = view.clone();
                Some(BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(1),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx: &mut BlockContext| {
                        render_side_arrow(index, side, view.clone(), cx)
                    }),
                    priority: 0,
                })
            })
            .collect::<Vec<_>>();
        let ids = editor.insert_blocks(blocks, None, cx);
        indices.into_iter().zip(ids).collect()
    })
}

fn render_side_arrow(
    index: usize,
    side: Side,
    view: WeakEntity<MergeView>,
    cx: &mut BlockContext,
) -> AnyElement {
    let label = match side {
        Side::Ours => "\u{00BB}",
        _ => "\u{00AB}",
    };
    let mut row = h_flex()
        .id(cx.block_id)
        .occlude()
        .h(cx.line_height)
        .gap_1()
        .bg(cx.theme().colors().editor_background);
    if matches!(side, Side::Ours) {
        row = row.w_full().justify_end().pr_1();
    }
    let view_revert = view.clone();
    row.child(
        Button::new(("side-revert", index), "\u{00D7}")
            .label_size(LabelSize::Small)
            .on_click(move |_, _window, cx| {
                view_revert
                    .update(cx, |view, cx| view.ignore_fragment(index, cx))
                    .ok();
            }),
    )
    .child(
        Button::new(("side-arrow", index), label)
            .label_size(LabelSize::Small)
            .on_click(move |_, _window, cx| {
                view.update(cx, |view, cx| view.accept_fragment(index, side, cx))
                    .ok();
            }),
    )
    .into_any_element()
}

fn highlight_changed_rows(
    editor: &Entity<Editor>,
    ranges: Vec<(usize, usize)>,
    color: fn(&App) -> Hsla,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        for (start, end) in ranges {
            let range = snapshot.anchor_before(MultiBufferOffset(start))
                ..snapshot.anchor_after(MultiBufferOffset(end));
            editor.highlight_rows::<ChangedSideHighlight>(
                range,
                color,
                RowHighlightOptions::default(),
                cx,
            );
        }
    });
}

struct ChangedSideHighlight;

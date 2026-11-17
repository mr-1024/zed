use anyhow::Context as _;
use git::{AbortMerge, MergeResultApplied, SmartMergeFile, repository::RepoPath};
use gpui::{AppContext as _, Context, Window};
use notifications::status_toast::StatusToast;
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{DetachAndPromptErr, NotifyTaskExt},
};

pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.register_action(
        |workspace, action: &SmartMergeFile, window, cx| {
            smart_merge_file(workspace, action, window, cx);
        },
    );
    workspace.register_action(|workspace, _: &AbortMerge, window, cx| {
        abort_merge(workspace, window, cx);
    });
    workspace.register_action(|workspace, action: &MergeResultApplied, _window, cx| {
        let message = if action.all_resolved {
            "所有更改均已处理，保存更改并完成合并"
        } else {
            "更改已保存，仍有未处理的冲突"
        };
        workspace.toggle_status_toast(StatusToast::new(message, cx, |this, _| this), cx);
    });
}

fn abort_merge(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let Some(repository) = project.read(cx).active_repository(cx) else {
        return;
    };
    let result = repository.update(cx, |repo, cx| repo.merge_abort(cx));

    cx.spawn_in(window, async move |workspace, cx| {
        result.await??;
        workspace
            .update_in(cx, |workspace, _window, cx| {
                workspace.toggle_status_toast(
                    StatusToast::new("已中止合并", cx, |this, _| this),
                    cx,
                );
            })
            .log_err();
        anyhow::Ok(())
    })
    .detach_and_prompt_err("中止合并失败", window, cx, |error, _, _| {
        Some(error.to_string())
    });
}

fn smart_merge_file(
    workspace: &mut Workspace,
    action: &SmartMergeFile,
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

    let stage_texts = repository
        .read(cx)
        .load_merge_stage_texts(repo_path, cx);

    cx.spawn_in(window, async move |workspace, cx| {
        let (base, ours, theirs) = stage_texts.await?;

        let (merged, has_conflicts) = match (ours, theirs) {
            (Some(ours), Some(theirs)) => {
                let base = base.unwrap_or_default();
                match diffy::merge(&base, &ours, &theirs) {
                    Ok(merged) => (merged, false),
                    Err(merged) => (merged, true),
                }
            }
            (Some(ours), None) => (ours, false),
            (None, Some(theirs)) => (theirs, false),
            (None, None) => anyhow::bail!("no merge stages found for this file"),
        };

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer
                .edit([(0..len, merged)], None, cx)
                .context("failed to write merged text")
        })?;

        workspace
            .update_in(cx, |workspace, window, cx| {
                let message = if has_conflicts {
                    "智能合并完成，仍有冲突待处理"
                } else {
                    "智能合并完成，全部更改已合并"
                };
                workspace.toggle_status_toast(StatusToast::new(message, cx, |this, _| this), cx);
                let _ = window;
            })
            .log_err();

        anyhow::Ok(())
    })
    .detach_and_prompt_err("智能合并失败", window, cx, |error, _, _| {
        Some(error.to_string())
    });
}

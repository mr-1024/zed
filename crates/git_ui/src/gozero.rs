use std::path::{Path, PathBuf};

use git::GenerateGoZeroZrpc;
use gpui::Context;
use notifications::status_toast::StatusToast;
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{DetachAndPromptErr, NotifyTaskExt},
};

pub(crate) fn register(workspace: &mut Workspace, _cx: &mut Context<Workspace>) {
    workspace.register_action(|workspace, action: &GenerateGoZeroZrpc, window, cx| {
        generate(workspace, action, window, cx);
    });
}

fn goctl_binary() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = Path::new(&home).join("go").join("bin").join("goctl");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("goctl")
}

fn generate(
    workspace: &mut Workspace,
    action: &GenerateGoZeroZrpc,
    window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
) {
    let proto_path = PathBuf::from(&action.path);
    let Some(proto_dir) = proto_path.parent().map(Path::to_path_buf) else {
        return;
    };
    let goctl = goctl_binary();
    let home_template = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".goctl"))
        .filter(|path| path.is_dir());

    cx.spawn_in(window, async move |workspace, cx| {
        let output = cx
            .background_executor()
            .spawn(async move {
                let mut command = std::process::Command::new(&goctl);
                command
                    .arg("rpc")
                    .arg("protoc")
                    .arg("-I")
                    .arg(&proto_dir)
                    .arg(&proto_path)
                    .arg("--style")
                    .arg("go_zero")
                    .arg("--zrpc_out")
                    .arg(&proto_dir)
                    .arg("--go_out")
                    .arg(&proto_dir)
                    .arg("--go-grpc_out")
                    .arg(&proto_dir);
                if let Some(home) = home_template {
                    command.arg("--home").arg(home);
                }
                command.output()
            })
            .await?;

        let succeeded = output.status.success();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if succeeded {
            "zRPC 代码生成完成".to_string()
        } else {
            let detail: String = stderr.chars().take(300).collect();
            format!("zRPC 代码生成失败：{}", detail)
        };
        workspace
            .update_in(cx, |workspace, _window, cx| {
                workspace.toggle_status_toast(StatusToast::new(message, cx, |this, _| this), cx);
            })
            .log_err();

        anyhow::Ok(())
    })
    .detach_and_prompt_err("zRPC 代码生成失败", window, cx, |error, _, _| {
        Some(error.to_string())
    });
}

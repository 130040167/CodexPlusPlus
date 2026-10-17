//! 管理器专用入口。协作配置不进入已有 settings 或第三方拓展路由。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use codex_plus_core::agent_collaboration::{
    CollaborationConfig, Store, TaskRequest, executor, mcp,
};
use serde_json::{Value, json};

#[tauri::command]
pub async fn agent_collaboration_request(
    action: String,
    payload: Option<Value>,
) -> Result<Value, String> {
    let store = Store::default();
    if action == "mcp_config" {
        return mcp_configuration(
            &std::env::current_exe().map_err(|e| e.to_string())?,
            &codex_plus_core::paths::default_app_state_dir(),
        )
        .map_err(|e| e.to_string());
    }
    let task_store = store.clone();
    let (snapshot, task_id) = tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<_> {
        let payload = payload.unwrap_or(Value::Null);
        let task_id = match action.as_str() {
            "snapshot" => None,
            "save_config" => {
                let config: CollaborationConfig =
                    serde_json::from_value(payload).context("协作配置格式无效")?;
                store.save_config(config)?;
                None
            }
            "submit" => {
                let request: TaskRequest =
                    serde_json::from_value(payload).context("任务参数格式无效")?;
                Some(store.submit(request)?.id)
            }
            "cancel" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .context("缺少任务 ID")?;
                store.cancel(id)?;
                None
            }
            _ => bail!("不支持的协作操作"),
        };
        Ok((store.snapshot()?, task_id))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if let Some(task_id) = task_id {
        executor::spawn_task(task_store, task_id);
    }
    Ok(snapshot)
}

fn mcp_configuration(executable: &Path, state_dir: &Path) -> anyhow::Result<Value> {
    if !executable.is_absolute() || !state_dir.is_absolute() {
        bail!("MCP 可执行文件与状态目录必须使用绝对路径");
    }
    let command = executable
        .to_str()
        .context("MCP 可执行路径不是有效 UTF-8")?;
    let state_dir = state_dir.to_str().context("MCP 状态目录不是有效 UTF-8")?;
    let args = ["--codex-plus-agent-mcp", "--state-dir", state_dir];
    let mut document = toml_edit::DocumentMut::new();
    document["mcp_servers"]["codex_plus_collaboration"]["command"] = toml_edit::value(command);
    let mut arguments = toml_edit::Array::new();
    for argument in args {
        arguments.push(argument);
    }
    document["mcp_servers"]["codex_plus_collaboration"]["args"] = toml_edit::value(arguments);
    let claude =
        json!({"mcpServers": {"codex_plus_collaboration": {"command": command, "args": args}}});
    Ok(json!({
        "command": command,
        "args": args,
        "codexToml": document.to_string(),
        "claudeJson": serde_json::to_string_pretty(&claude)?,
    }))
}

fn mcp_store_from_args(args: &[OsString]) -> anyhow::Result<Store> {
    match args {
        [] => Ok(Store::default()),
        [flag, directory] if flag == "--state-dir" => {
            let directory = PathBuf::from(directory);
            if !directory.is_absolute() {
                bail!("--state-dir 必须是绝对目录");
            }
            Ok(Store::new(directory.join("agent-collaboration.sqlite3")))
        }
        _ => bail!("MCP 模式仅接受可选的 --state-dir <绝对目录>"),
    }
}

pub fn run_mcp(args: &[OsString]) -> anyhow::Result<()> {
    let store = mcp_store_from_args(args)?;
    tauri::async_runtime::block_on(mcp::serve(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_configuration_round_trips_spaces_quotes_and_shared_state() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir
            .path()
            .join("Codex++ \"local\".app")
            .join("CodexPlusPlus");
        let state_dir = dir.path().join("collaboration state");
        let config = mcp_configuration(&executable, &state_dir).unwrap();
        let codex: toml_edit::DocumentMut = config["codexToml"].as_str().unwrap().parse().unwrap();
        let server = &codex["mcp_servers"]["codex_plus_collaboration"];
        assert_eq!(server["command"].as_str(), executable.to_str());
        assert_eq!(
            server["args"].as_array().unwrap().get(2).unwrap().as_str(),
            state_dir.to_str()
        );
        let claude: Value = serde_json::from_str(config["claudeJson"].as_str().unwrap()).unwrap();
        assert_eq!(
            claude["mcpServers"]["codex_plus_collaboration"]["args"],
            config["args"]
        );
    }

    #[test]
    fn mcp_mode_rejects_unknown_or_relative_arguments_before_starting() {
        for args in [
            vec!["--state-dir"],
            vec!["--state-dir", "relative"],
            vec!["--execute", "anything"],
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert!(mcp_store_from_args(&args).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(
            mcp_store_from_args(&[
                OsString::from("--state-dir"),
                dir.path().as_os_str().to_owned()
            ])
            .is_ok()
        );
    }
}

//! 管理器专用入口。协作配置不进入已有 settings 或第三方拓展路由。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use codex_plus_core::agent_collaboration::{
    CollaborationConfig, Store, TaskRequest, executor, integration, mcp,
};
use codex_plus_core::settings::SettingsStore;
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
        let executable = std::env::current_exe().context("无法定位子 Agent MCP 程序")?;
        let home = codex_plus_core::codex_home::default_codex_home_dir();
        let state_dir = codex_plus_core::paths::default_app_state_dir();
        let task_id = match action.as_str() {
            "snapshot" => None,
            "connect_codex" => {
                connect_codex(&SettingsStore::default(), &home, &executable, &state_dir)?;
                None
            }
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
        let mut snapshot = store.snapshot()?;
        snapshot["integration"] = registration_view(&home, &executable, &state_dir);
        Ok((snapshot, task_id))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if let Some(task_id) = task_id {
        executor::spawn_task(task_store, task_id);
    }
    Ok(snapshot)
}

fn registration_view(home: &Path, executable: &Path, state_dir: &Path) -> Value {
    match integration::status(home, executable, state_dir) {
        Ok(registration) => json!({
            "status": if registration.registered { "connected" } else if registration.status == "conflict" { "conflict" } else { "not_connected" },
            "message": registration.message,
            "home": registration.home,
            "configPath": registration.config_path,
        }),
        Err(error) => json!({"status": "not_connected", "message": error.to_string()}),
    }
}

/// 只更新协作 MCP 这一条。使用既有局部更新 API，保留其他设置与供应商数据。
fn connect_codex(
    settings_store: &SettingsStore,
    home: &Path,
    executable: &Path,
    state_dir: &Path,
) -> anyhow::Result<()> {
    let registration = integration::status(home, executable, state_dir)?;
    if registration.status == "conflict" {
        bail!("同名协作 MCP 属于其他配置，未覆盖");
    }
    let settings = settings_store
        .load()
        .context("无法读取 Codex++ 托管配置，未接入")?;
    let before = settings.relay_context_config_contents.clone();
    let updated = integration::upsert_managed_context(&before, executable, state_dir)?;
    let changed = updated != before;
    let mut committed_context = updated.clone();
    if changed {
        let committed = settings_store
            .update(context_patch(&settings, &updated)?)
            .context("无法保存子 Agent 托管配置，未修改 Codex live 配置")?;
        // 设置存储会归一化 TOML，以实际提交结果作为回滚的比较基准。
        committed_context = committed.relay_context_config_contents;
    }
    if let Err(error) = integration::install(home, executable, state_dir) {
        if changed {
            // 重新读取，恢复的仅是本次写入的上下文字段，不能覆盖期间的其他设置变化。
            let rollback = (|| -> anyhow::Result<()> {
                let latest = settings_store.load()?;
                if latest.relay_context_config_contents != committed_context {
                    bail!("托管 MCP 已被其他操作修改，未覆盖新数据");
                }
                settings_store
                    .update(context_patch(&latest, &before)?)
                    .map(|_| ())
            })();
            if rollback.is_err() {
                bail!("接入 Codex 失败：{error}；托管配置未能恢复，请刷新 MCP 设置后检查");
            }
        }
        return Err(error).context("子 Agent 接入失败，托管配置已保持原状");
    }
    Ok(())
}

fn context_patch(
    settings: &codex_plus_core::settings::BackendSettings,
    contents: &str,
) -> anyhow::Result<Value> {
    let mut patch = json!({"relayContextConfigContents": contents});
    // 较新的设置存储支持条件更新；通过序列化元数据兼容已有公开 API。
    if let Some(revision) = serde_json::to_value(settings)?
        .get("revision")
        .filter(|value| value.is_string())
    {
        patch["revision"] = revision.clone();
    }
    Ok(patch)
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

    #[test]
    fn connect_codex_persists_the_same_server_in_managed_and_live_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let executable = std::env::current_exe().unwrap();
        let state_dir = dir.path().join("state");
        let settings = SettingsStore::new(dir.path().join("settings.json"));
        let mut raw =
            serde_json::to_value(codex_plus_core::settings::BackendSettings::default()).unwrap();
        raw["futurePreference"] = json!({"keep": true});
        std::fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
        connect_codex(&settings, &home, &executable, &state_dir).unwrap();
        let managed = settings.load().unwrap().relay_context_config_contents;
        assert!(managed.contains("codex_plus_collaboration"));
        assert!(managed.contains("--codex-plus-agent-mcp"));
        assert_eq!(
            registration_view(&home, &executable, &state_dir)["status"],
            "connected"
        );
        connect_codex(&settings, &home, &executable, &state_dir).unwrap();
        assert_eq!(
            settings.load().unwrap().relay_context_config_contents,
            managed
        );
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(persisted["futurePreference"], json!({"keep": true}));
    }

    #[cfg(unix)]
    #[test]
    fn failed_live_publish_restores_only_the_managed_context() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let settings = SettingsStore::new(dir.path().join("settings.json"));
        let mut before = codex_plus_core::settings::BackendSettings::default();
        before.relay_context_config_contents =
            "[mcp_servers.existing]\ncommand = \"existing-tool\"\n".to_string();
        settings.save(&before).unwrap();
        let original = settings.load().unwrap().relay_context_config_contents;
        let target = dir.path().join("not-a-lock");
        std::fs::write(&target, "leave intact").unwrap();
        std::os::unix::fs::symlink(&target, home.join(".codex-plus-agent-mcp.lock")).unwrap();
        assert!(
            connect_codex(
                &settings,
                &home,
                &std::env::current_exe().unwrap(),
                &dir.path().join("state")
            )
            .is_err()
        );
        assert_eq!(
            settings.load().unwrap().relay_context_config_contents,
            original
        );
        assert!(!home.join("config.toml").exists());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "leave intact");
    }
}

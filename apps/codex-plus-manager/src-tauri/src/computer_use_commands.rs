//! Computer Use 接入命令。
//!
//! 只暴露给管理器前端；不注册到用户脚本路由，也不经过 HTTP 层。
//! 具体行为都在 `codex_plus_core::computer_use` 里，这里只做参数解析与提示文案。

use serde_json::{Value, json};

use crate::commands::{CommandResult, failed, ok};

async fn run_blocking<T, F>(run: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(run)
        .await
        .map_err(|error| format!("任务执行失败：{error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn computer_use_status() -> CommandResult<Value> {
    match run_blocking(codex_plus_core::computer_use::status).await {
        Ok(report) => {
            let message = if report.runtime_found {
                "已找到 Codex 运行时。"
            } else {
                "未找到 ChatGPT.app，Computer Use 暂时不可接入。"
            };
            ok(message, serde_json::to_value(report).unwrap_or_else(|_| json!({})))
        }
        Err(error) => failed(&error, json!({})),
    }
}

#[tauri::command]
pub async fn computer_use_install(target: String) -> CommandResult<Value> {
    let parsed = match codex_plus_core::computer_use::Target::parse(&target) {
        Ok(target) => target,
        Err(error) => return failed(&error.to_string(), json!({})),
    };
    match run_blocking(move || codex_plus_core::computer_use::install(parsed)).await {
        Ok(receipt) => {
            let message = if receipt.changed {
                "已接入；重启对应客户端后生效。"
            } else {
                "已是接入状态，无需改动。"
            };
            ok(
                message,
                serde_json::to_value(receipt).unwrap_or_else(|_| json!({})),
            )
        }
        Err(error) => failed(&error, json!({})),
    }
}

#[tauri::command]
pub async fn computer_use_uninstall(target: String) -> CommandResult<Value> {
    let parsed = match codex_plus_core::computer_use::Target::parse(&target) {
        Ok(target) => target,
        Err(error) => return failed(&error.to_string(), json!({})),
    };
    match run_blocking(move || codex_plus_core::computer_use::uninstall(parsed)).await {
        Ok(receipt) => {
            let message = if receipt.changed {
                "已移除接入；原配置已备份。"
            } else {
                "本来就没有接入，未改动配置。"
            };
            ok(
                message,
                serde_json::to_value(receipt).unwrap_or_else(|_| json!({})),
            )
        }
        Err(error) => failed(&error, json!({})),
    }
}

#[tauri::command]
pub async fn computer_use_check() -> CommandResult<Value> {
    match run_blocking(|| Ok(codex_plus_core::computer_use::check())).await {
        Ok(report) => {
            let message = if report.ok {
                format!(
                    "自检通过：识别到 {} 个应用，用时 {} ms。",
                    report.app_count.unwrap_or(0),
                    report.duration_ms
                )
            } else {
                report
                    .failure_detail
                    .clone()
                    .unwrap_or_else(|| "自检失败。".to_string())
            };
            ok(
                &message,
                serde_json::to_value(report).unwrap_or_else(|_| json!({})),
            )
        }
        Err(error) => failed(&error, json!({})),
    }
}

/// 打开 ChatGPT.app。运行时缺失时给用户一个明确的下一步动作。
#[tauri::command]
pub async fn computer_use_open_app() -> CommandResult<Value> {
    let result = run_blocking(|| -> anyhow::Result<String> {
        let runtime = codex_plus_core::computer_use::locate_runtime()?
            .ok_or_else(|| anyhow::anyhow!("未找到 ChatGPT.app"))?;
        Ok(runtime.app_path)
    })
    .await;
    match result {
        Ok(app_path) => {
            let status = std::process::Command::new("open")
                .arg(&app_path)
                .status();
            match status {
                Ok(status) if status.success() => ok(
                    "已打开 ChatGPT；请确认 Computer Use 的辅助功能与屏幕录制权限已授予。",
                    json!({ "appPath": app_path }),
                ),
                Ok(status) => failed(
                    &format!("打开 ChatGPT 失败（退出码 {:?}）", status.code()),
                    json!({ "appPath": app_path }),
                ),
                Err(error) => failed(&format!("打开 ChatGPT 失败：{error}"), json!({})),
            }
        }
        Err(error) => failed(&error, json!({})),
    }
}

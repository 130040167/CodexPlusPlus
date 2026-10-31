use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use super::route;
use super::store::GatewaySettings;
use crate::protocol_proxy::ProxySessionHeaders;
use crate::tools::ToolId;

const POLICY_MARKER: &str = "[Codex++ inherited model]";

struct Identity {
    thread: String,
    turn: String,
    parent: String,
    parent_turn: String,
    child: bool,
}

impl Identity {
    fn read(headers: ProxySessionHeaders<'_>) -> anyhow::Result<Self> {
        let header = |name: &str| {
            headers
                .client_identity
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let metadata: Value = header("x-codex-turn-metadata")
            .map(serde_json::from_str)
            .transpose()
            .context("Codex 回合身份格式无效")?
            .unwrap_or(Value::Null);
        let field = |name: &str| metadata[name].as_str().unwrap_or("").trim().to_string();
        let checked = |value: Option<&str>, name: &str| -> anyhow::Result<String> {
            let value = value.unwrap_or("").trim();
            let meta = field(name);
            if !value.is_empty() && !meta.is_empty() && value != meta {
                bail!("Codex 请求头与回合身份不一致：{name}");
            }
            let value = if value.is_empty() {
                meta
            } else {
                value.to_string()
            };
            if value.len() > 256 || value.chars().any(char::is_control) {
                bail!("Codex 回合身份无效：{name}");
            }
            Ok(value)
        };
        let parent = checked(header("x-codex-parent-thread-id"), "parent_thread_id")?;
        let child = !parent.is_empty()
            || header("x-openai-subagent") == Some("collab_spawn")
            || metadata["subagent_kind"] == "thread_spawn";
        let thread = checked(headers.thread_id, "thread_id")?;
        let thread = if thread.is_empty() {
            checked(headers.session_id, "thread_id")?
        } else {
            thread
        };
        Ok(Self {
            thread,
            turn: checked(None, "turn_id")?,
            parent,
            parent_turn: checked(None, "parent_turn_id")?,
            child,
        })
    }
}

fn recorded_model(
    db: &Connection,
    scope: &str,
    thread: &str,
    turn: &str,
) -> anyhow::Result<Option<(String, String, String)>> {
    Ok(db
        .query_row(
            "SELECT model, parent, parent_turn FROM gateway_subagent_models
         WHERE scope=?1 AND thread=?2 AND turn=?3",
            params![scope, thread, turn],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

fn parent_rollout_model(
    home: &Path,
    sqlite_home: &Path,
    parent: &str,
    turn: &str,
) -> anyhow::Result<String> {
    for path in crate::codex_sqlite::codex_session_db_paths_in_home(sqlite_home) {
        let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        database.busy_timeout(Duration::from_secs(2))?;
        let record = database
            .query_row(
                "SELECT rollout_path, source FROM threads WHERE id=?1",
                [parent],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((rollout_path, source)) = record else {
            continue;
        };
        if serde_json::from_str::<Value>(&source)
            .ok()
            .is_some_and(|source| source.get("subagent").is_some())
        {
            bail!("父回合来自子智能体，缺少其已继承的模型身份，不能按裸模型重新选择供应商");
        }
        let rollout = std::fs::canonicalize(rollout_path).context("父回合记录不可读取")?;
        if !rollout.starts_with(home) {
            bail!("父回合记录不在当前 Codex Home 中");
        }
        for line in BufReader::new(File::open(rollout)?).lines() {
            let line = line?;
            if !line.contains("\"turn_context\"") {
                continue;
            }
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if row["type"] == "turn_context" && row["payload"]["turn_id"] == turn {
                let model = row["payload"]["model"].as_str().unwrap_or("").trim();
                if !model.is_empty() {
                    return Ok(model.to_string());
                }
            }
        }
    }
    bail!("找不到父回合的模型身份，拒绝回退到默认供应商")
}

fn patch_spawn_tools(tools: &mut Value, model: &str, native: bool) {
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("").to_string();
        let native = native
            || matches!(
                name.as_str(),
                "multi_agent_v1" | "multi_agent_v2" | "multi_agent"
            );
        if tool["type"] == "namespace" {
            patch_spawn_tools(&mut tool["tools"], model, native);
        } else if (native && name == "spawn_agent")
            || matches!(
                name.as_str(),
                "multi_agent_v1.spawn_agent" | "multi_agent_v2.spawn_agent"
            )
        {
            let original = tool["description"].as_str().unwrap_or("");
            let original = original
                .split(POLICY_MARKER)
                .next()
                .unwrap_or("")
                .trim_end();
            tool["description"] = Value::String(format!(
                "{original}\n{POLICY_MARKER} 子智能体继承父回合完整模型身份 {model}；创建时省略 model 参数。供应商前缀和 group/ 聚合组都是有效身份，不能改写成裸模型或一次选中的上游。"
            ));
        }
    }
}

pub(super) fn prepare(
    gateway: &GatewaySettings,
    agent: &ToolId,
    request: &mut Value,
    headers: ProxySessionHeaders<'_>,
) -> anyhow::Result<()> {
    let home = crate::codex_home::default_codex_home_dir();
    let sqlite_home = crate::codex_sqlite::resolve_sqlite_home_home_or_default(&home);
    prepare_at(
        gateway,
        agent,
        request,
        headers,
        &crate::paths::default_app_state_dir().join("gateway-subagent-models.sqlite"),
        &home,
        &sqlite_home,
    )
}

fn prepare_at(
    gateway: &GatewaySettings,
    agent: &ToolId,
    request: &mut Value,
    headers: ProxySessionHeaders<'_>,
    path: &Path,
    home: &Path,
    sqlite_home: &Path,
) -> anyhow::Result<()> {
    if *agent != ToolId::Codex {
        return Ok(());
    }
    let Some(binding) = gateway.active_binding(agent) else {
        return Ok(());
    };
    let identity = Identity::read(headers)?;
    if identity.child
        && [
            &identity.thread,
            &identity.turn,
            &identity.parent,
            &identity.parent_turn,
        ]
        .iter()
        .any(|value| value.is_empty())
    {
        bail!("子智能体缺少完整父回合身份，拒绝按默认供应商路由");
    }
    if identity.thread.is_empty() || identity.turn.is_empty() {
        return Ok(());
    }
    if identity.child && identity.thread == identity.parent {
        bail!("子智能体不能把自己声明为父回合");
    }
    let home = std::fs::canonicalize(home)?;
    let sqlite_home = std::fs::canonicalize(sqlite_home)?;
    let scope = serde_json::to_string(&(&home, &sqlite_home))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?
    }
    let database = Connection::open(path)?;
    database.busy_timeout(Duration::from_secs(2))?;
    database.execute_batch(
        "CREATE TABLE IF NOT EXISTS gateway_subagent_models (
            scope TEXT NOT NULL, thread TEXT NOT NULL, turn TEXT NOT NULL,
            parent TEXT NOT NULL, parent_turn TEXT NOT NULL, model TEXT NOT NULL,
            PRIMARY KEY(scope, thread, turn)
        ) WITHOUT ROWID;",
    )?;
    let saved = recorded_model(&database, &scope, &identity.thread, &identity.turn)?;
    let model = if let Some((model, parent, parent_turn)) = saved {
        if parent != identity.parent || parent_turn != identity.parent_turn {
            bail!("子智能体父回合身份与已记录的身份不一致");
        }
        model
    } else {
        let model = if identity.child {
            match recorded_model(&database, &scope, &identity.parent, &identity.parent_turn)? {
                Some((model, _, _)) => model,
                None => parent_rollout_model(
                    &home,
                    &sqlite_home,
                    &identity.parent,
                    &identity.parent_turn,
                )?,
            }
        } else {
            route::effective_model(
                gateway,
                binding,
                &route::binding_default_model(gateway, binding),
                request["model"].as_str().unwrap_or(""),
            )
        };
        database.execute(
            "INSERT INTO gateway_subagent_models(scope,thread,turn,parent,parent_turn,model)
             VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(scope,thread,turn) DO NOTHING",
            params![
                scope,
                identity.thread,
                identity.turn,
                identity.parent,
                identity.parent_turn,
                model
            ],
        )?;
        let (model, parent, parent_turn) =
            recorded_model(&database, &scope, &identity.thread, &identity.turn)?
                .context("模型身份记录未完成")?;
        if parent != identity.parent || parent_turn != identity.parent_turn {
            bail!("子智能体父回合身份与已记录的身份不一致");
        }
        model
    };
    let routable = if let Some(group) = model.strip_prefix("group/") {
        gateway
            .groups
            .iter()
            .any(|entry| entry.id.eq_ignore_ascii_case(group))
    } else {
        route::model_is_routable(gateway, binding, &model)
    };
    if identity.child && !routable {
        bail!("父回合模型 {model} 当前不可路由，拒绝隐式切换供应商");
    }
    request["model"] = Value::String(model.clone());
    if let Some(tools) = request.get_mut("tools") {
        patch_spawn_tools(tools, &model, false);
    }
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if item["type"] == "tool_search_output" {
                patch_spawn_tools(&mut item["tools"], &model, false);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "subagent_models_tests.rs"]
mod tests;

//! 专用的本机 MCP stdio 服务。协议数据是 stdout 的唯一输出。

use std::io::{BufRead, Write};

use anyhow::Context;
use serde_json::{Value, json};

use super::{Store, TaskRequest, executor};

pub const MAX_REQUEST_BYTES: usize = 128 * 1024;

enum InputLine {
    Data(Vec<u8>),
    TooLong,
    Failed,
}

/// 不启用 tokio io-std；有界读取放到专用 blocking 线程，不阻塞 CLI worker。
pub async fn serve(store: Store) -> anyhow::Result<()> {
    let result = serve_loop(store).await;
    // EOF、协议输出失败和输入读取失败均收口到这里，只停止此进程拥有的任务。
    executor::shutdown_owned_tasks().await;
    result
}

async fn serve_loop(store: Store) -> anyhow::Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            match read_bounded_line(&mut reader) {
                Ok(None) => break,
                Ok(Some(line)) => {
                    if sender.blocking_send(line).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.blocking_send(InputLine::Failed);
                    break;
                }
            }
        }
    });
    while let Some(line) = receiver.recv().await {
        let response = match line {
            InputLine::Data(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            InputLine::Data(line) => process_line(&store, &line),
            InputLine::TooLong => Some(rpc_error(Value::Null, -32600, "请求行超过 128 KiB 上限")),
            InputLine::Failed => anyhow::bail!("无法读取 MCP stdin"),
        };
        if let Some(response) = response {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer(&mut output, &response).context("无法写入 MCP 响应")?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn read_bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<InputLine>> {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut consumed_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if !consumed_any {
                None
            } else if too_long {
                Some(InputLine::TooLong)
            } else {
                Some(InputLine::Data(line))
            });
        }
        consumed_any = true;
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|position| position + 1)
            .unwrap_or(available.len());
        let ended = available[take - 1] == b'\n';
        if !too_long {
            if line.len().saturating_add(take) > MAX_REQUEST_BYTES {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if ended {
            return Ok(Some(if too_long {
                InputLine::TooLong
            } else {
                InputLine::Data(line)
            }));
        }
    }
}

/// 测试及嵌入时沿用与 stdio 完全相同的错误语义。
pub fn process_line(store: &Store, line: &[u8]) -> Option<Value> {
    if line.len() > MAX_REQUEST_BYTES {
        return Some(rpc_error(Value::Null, -32600, "请求行超过 128 KiB 上限"));
    }
    match serde_json::from_slice(line) {
        Ok(request) => handle_request(store, request),
        Err(_) => Some(rpc_error(Value::Null, -32700, "JSON 解析失败")),
    }
}

pub fn handle_request(store: &Store, request: Value) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(rpc_error(Value::Null, -32600, "请求必须是 JSON-RPC 对象"));
    };
    // 通知没有响应，包括未知通知；绝不把通知误当成工具执行请求。
    let Some(id) = object.get("id").cloned() else {
        return None;
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !(id.is_string() || id.is_number() || id.is_null())
    {
        return Some(rpc_error(Value::Null, -32600, "JSON-RPC 请求格式无效"));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(id, -32600, "缺少请求方法"));
    };
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            let version = if matches!(requested, "2024-11-05" | "2025-03-26" | "2025-06-18") {
                requested
            } else {
                "2025-06-18"
            };
            json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "codex-plus-agent-collaboration", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "仅执行用户明确授权的 CLI 协作任务。任务可修改指定工作目录并消耗各 CLI 账户额度；权限由 CLI 自身控制。不能控制或唤醒已有桌面会话。send_task 返回 queued 只代表记录成功，随后用 get_task 查询终态；结果不确定时不要自动重试。"})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_definitions()}),
        "tools/call" => match params.get("name").and_then(Value::as_str) {
            Some(name) => tool_result(call_tool(
                store,
                name,
                params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            )),
            None => return Some(rpc_error(id, -32602, "tools/call 缺少工具名")),
        },
        _ => return Some(rpc_error(id, -32601, "未实现的 MCP 方法")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_result(result: anyhow::Result<Value>) -> Value {
    match result {
        Ok(value) => {
            json!({"content": [{"type": "text", "text": value.to_string()}], "isError": false})
        }
        Err(error) => {
            json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true})
        }
    }
}

fn required_id(arguments: &Value) -> anyhow::Result<&str> {
    arguments
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .context("taskId 必须是任务返回的 ID")
}

fn call_tool(store: &Store, name: &str, arguments: Value) -> anyhow::Result<Value> {
    if !arguments.is_object() {
        anyhow::bail!("工具参数必须是对象");
    }
    match name {
        "list_agents" => {
            let snapshot = store.snapshot()?;
            let enabled = snapshot["config"]["enabled"].as_bool().unwrap_or(false);
            let agents: Vec<Value> = snapshot["config"]["agents"].as_array().into_iter().flatten().map(|agent| {
                let available = snapshot["availability"].as_array().into_iter().flatten()
                    .find(|entry| entry["id"] == agent["id"]).and_then(|entry| entry["available"].as_bool()).unwrap_or(false);
                json!({"id": agent["id"], "name": agent["name"], "enabled": agent["enabled"], "available": available})
            }).collect();
            Ok(
                json!({"enabled": enabled, "agents": agents, "desktopSessionWake": false,
                "availabilityMeaning": "available 仅表示找到可执行文件，未验证登录或协议兼容"}),
            )
        }
        "send_task" => {
            let request: TaskRequest =
                serde_json::from_value(arguments).context("send_task 参数无效")?;
            let task = store.submit(request)?;
            executor::spawn_task(store.clone(), task.id.clone());
            Ok(json!({"taskId": task.id, "task": task,
                "next": "继续独立工作，随后使用 get_task 查询 taskId；不要重复 send_task。"}))
        }
        "get_task" => Ok(serde_json::to_value(store.task(required_id(&arguments)?)?)?),
        "cancel_task" => Ok(serde_json::to_value(
            store.cancel(required_id(&arguments)?)?,
        )?),
        "list_tasks" => {
            let limit = arguments
                .get("limit")
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|limit| *limit > 0 && *limit <= 50)
                        .context("limit 必须在 1 到 50 之间")
                })
                .transpose()?
                .unwrap_or(20) as usize;
            let mut snapshot = store.snapshot()?;
            let tasks = snapshot["tasks"]
                .as_array_mut()
                .context("协作任务列表不可用")?;
            tasks.truncate(limit);
            Ok(json!({"tasks": tasks, "limit": limit}))
        }
        _ => anyhow::bail!("未知协作工具"),
    }
}

fn tool_definitions() -> Vec<Value> {
    let id_schema = json!({"type": "object", "properties": {"taskId": {"type": "string"}}, "required": ["taskId"], "additionalProperties": false});
    vec![
        json!({"name": "list_agents", "description": "只读：列出固定 CLI worker 与启用状态。available 只验证本机文件，不代表登录成功。配置必须由用户在 Codex++ 中修改。", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}, "annotations": {"readOnlyHint": true}}),
        json!({"name": "send_task", "description": "需要用户授权：异步启动真实 CLI，可能编辑指定目录并消耗账户额度。默认新建 CLI 会话；continueTaskId 只续由本工具创建的同 agent、同目录且有会话 ID 的已完成任务。返回 taskId 后继续独立工作，随后 get_task 查询；queued 不代表成功。每个 agent 仅允许一个活跃任务，busy 时不自动重试。不唤醒已有桌面会话。", "inputSchema": {"type": "object", "properties": {"agentId": {"type": "string", "enum": ["codex", "deepseek", "claude"]}, "prompt": {"type": "string", "maxLength": 65536}, "cwd": {"type": "string", "description": "已存在的绝对工作目录"}, "continueTaskId": {"type": ["string", "null"]}}, "required": ["agentId", "prompt", "cwd"], "additionalProperties": false}, "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false}}),
        json!({"name": "get_task", "description": "只读：查询任务持久化状态与输出。completed/failed/cancelled/interrupted 为终态；interrupted 表示结果不确定，不应自动重发。", "inputSchema": id_schema, "annotations": {"readOnlyHint": true}}),
        json!({"name": "list_tasks", "description": "只读：活跃任务优先，随后列出最近的持久化协作任务，最多 50 条；不会启动 agent。", "inputSchema": {"type": "object", "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 50}}, "additionalProperties": false}, "annotations": {"readOnlyHint": true}}),
        json!({"name": "cancel_task", "description": "请求停止指定任务。排队任务立即取消；运行任务等待 worker 停止 CLI 后结束。取消不会回滚已经发生的文件或外部操作。", "inputSchema": id_schema, "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_line_is_drained_without_losing_next_request() {
        let mut bytes = vec![b'x'; MAX_REQUEST_BYTES + 32];
        bytes.extend_from_slice(b"\n{}\n");
        let mut reader = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_bounded_line(&mut reader).unwrap(),
            Some(InputLine::TooLong)
        ));
        assert!(
            matches!(read_bounded_line(&mut reader).unwrap(), Some(InputLine::Data(line)) if line == b"{}\n")
        );
        assert!(read_bounded_line(&mut reader).unwrap().is_none());
    }
}

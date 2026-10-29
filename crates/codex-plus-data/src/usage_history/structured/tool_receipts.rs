//! 只保留执行状态和技能加载证据，不把工具正文转发给扩展。
use super::*;

fn failed_text(text: &str) -> bool {
    let text = text.trim_start().to_ascii_lowercase();
    [
        "error",
        "tool call error:",
        "tool execution failed",
        "tool call failed",
        "failed to call",
        "failed to execute",
        "mcp error",
        "error calling tool",
        "filenotfound",
        "cannot find path",
        "no such file",
        "access denied",
        "access is denied",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub(super) fn compact(value: &Value, depth: usize, skill_read: bool) -> Value {
    if depth > 5 {
        return Value::Null;
    }
    match value {
        Value::String(text) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                return compact(&parsed, depth + 1, skill_read);
            }
            let mut evidence = Vec::new();
            // 仅识别执行器回执头；正文中的同名行不能伪造退出状态。
            if exec_receipt(value).is_some() {
                for line in text.trim().lines() {
                    if let Some(seconds) = line.strip_prefix("Wall time: ") {
                        if let Some(seconds) = seconds.strip_suffix(" seconds") {
                            if let Ok(seconds) = seconds.parse::<f64>() {
                                if seconds.is_finite() && seconds >= 0.0 {
                                    evidence.push(format!("Wall time: {seconds} seconds"));
                                }
                            }
                        }
                    } else if let Some(code) = line.strip_prefix("Process exited with code ") {
                        if let Some(code) = code
                            .split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<i32>().ok())
                        {
                            evidence.push(format!("Process exited with code {code}"));
                        }
                        break;
                    } else if let Some(id) = line.strip_prefix("Process running with session ID ") {
                        if let Some(id) = id
                            .split_whitespace()
                            .next()
                            .filter(|v| v.bytes().all(|c| c.is_ascii_digit()))
                        {
                            evidence.push(format!("Process running with session ID {id}"));
                        }
                        break;
                    }
                }
            } else if failed_text(text) {
                evidence.push("Error: tool execution failed".into());
            } else if [
                "status is unknown",
                "outcome is unknown",
                "did not complete",
            ]
            .iter()
            .any(|status| text.trim().to_ascii_lowercase().starts_with(status))
            {
                return json!({"status":"unknown"});
            }
            // 用于批量/异步技能读取的结果判定，仅返回名称和“见过文档”标志。
            for line in text.lines().filter(|_| skill_read) {
                if let Some(name) = line.trim().strip_prefix("name:") {
                    let name = name.trim().trim_matches(['"', '\'']);
                    if !name.is_empty()
                        && name.len() <= 128
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || "._-:".contains(c))
                        && evidence.len() < 16
                    {
                        evidence.push(format!("name: {name}"));
                    }
                }
            }
            if skill_read
                && text.lines().any(|line| {
                    let line = line.trim_start();
                    line.starts_with("# ") || line.starts_with("## ") || line == "---"
                })
            {
                evidence.push("# Skill content observed".into());
            }
            json!(evidence.join("\n"))
        }
        Value::Array(parts) => json!(
            parts
                .iter()
                .take(32)
                .map(|part| compact(part, depth + 1, skill_read))
                .collect::<Vec<_>>()
        ),
        Value::Object(object) => {
            let mut result = serde_json::Map::new();
            for key in ["isError", "success"] {
                if let Some(value) = object.get(key).and_then(Value::as_bool) {
                    result.insert(key.into(), json!(value));
                }
            }
            if object
                .get("error")
                .is_some_and(|v| !v.is_null() && *v != false)
            {
                result.insert("error".into(), json!(true));
            }
            for key in ["exit_code", "exitCode"] {
                if let Some(value) = object.get(key).and_then(Value::as_i64) {
                    result.insert(key.into(), json!(value));
                }
            }
            if let Some(status) = object
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| {
                    matches!(
                        *status,
                        "success"
                            | "completed"
                            | "failed"
                            | "error"
                            | "cancelled"
                            | "canceled"
                            | "interrupted"
                            | "unknown"
                            | "pending"
                            | "running"
                            | "in_progress"
                    )
                })
            {
                result.insert("status".into(), json!(status));
            }
            if let Some(kind) = object
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| matches!(*kind, "text" | "input_text" | "output_text"))
            {
                result.insert("type".into(), json!(kind));
                result.insert(
                    "text".into(),
                    compact(&object["text"], depth + 1, skill_read),
                );
            }
            if let Some(content) = object.get("content").filter(|v| v.is_array()) {
                result.insert("content".into(), compact(content, depth + 1, skill_read));
            }
            Value::Object(result)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_bodies_are_removed_but_status_and_skill_evidence_remain() {
        let secret = "private-api-key-and-document-body";
        for value in [
            json!(secret),
            json!({"content":[{"type":"text","text":secret}],"error":secret}),
            json!(format!(
                "Process exited with code 0\nOutput:\n---\nname: pdf\n---\n{secret}"
            )),
            json!([{"type":"input_text","text":format!("Error executing tool: {secret}")}]),
        ] {
            assert!(!compact(&value, 0, true).to_string().contains(secret));
        }
        assert_eq!(
            compact(&json!({"success":false,"error":"secret"}), 0, false),
            json!({"success":false,"error":true})
        );
        assert_eq!(
            compact(
                &json!(
                    "Wall time: 2.5 seconds\nProcess exited with code 1\nOutput:\nname: pdf\n# Private body"
                ),
                0,
                true
            ),
            json!(
                "Wall time: 2.5 seconds\nProcess exited with code 1\nname: pdf\n# Skill content observed"
            )
        );
        assert_eq!(
            compact(&json!("name: private-person\n# private-document"), 0, false),
            json!("")
        );
    }
}

//! 别家签发的加密推理（#2777）。
//!
//! Codex 每轮都把历史里的 reasoning 项（带 `encrypted_content`）原样回传。同一个对话先用中转、
//! 再换到官方账号（或反过来）时，这些推理内容是上一家签发的，新上游校验不过就整条请求 400：
//! 「The encrypted content for item rs_… could not be verified」。
//!
//! 处理方式偏保守：只有上游明确报了这类错误，才去掉请求里的加密推理项重发一次，
//! 并按「上游 + 内容摘要」记住被拒的项，同一对话后面的请求发往这一家时提前去掉，不再每轮先吃一次 400。
//! 推理项只是模型的思考过程，去掉后上游照常按消息与工具历史继续；没被拒过的请求一律不动。

use std::collections::{HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 记住的被拒推理项上限（按上游 + 内容摘要），超出先忘最早的。
const MEMORY_LIMIT: usize = 4096;

/// 请求日志里这次重试的原因。
pub const RETRY_REASON: &str = "invalid_encrypted_content";

type MemoryKey = (String, u64);

#[derive(Default)]
struct Memory {
    keys: HashSet<MemoryKey>,
    order: VecDeque<MemoryKey>,
}

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: OnceLock<Mutex<Memory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory::default()))
}

/// 带不透明推理内容的 reasoning 项返回它的内容。
fn encrypted_content(item: &Value) -> Option<&str> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// 按内容摘要识别同一个推理项：经网关发往 ChatGPT 后端时项 id 会被去掉，不能按 id 记。
fn digest(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// 请求里有没有带加密内容的推理项：没有时上游的 400 与这里无关，不必预读错误体。
pub fn has_encrypted_reasoning(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| encrypted_content(item).is_some()))
}

/// 上游的错误是不是「加密推理内容校验不过」：OpenAI 的错误码 `invalid_encrypted_content`，
/// 或中转换了包装、只留下原文时的报错文字。
pub fn is_unverifiable_reasoning_rejection(status: u16, body: &[u8]) -> bool {
    if status != 400 {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("invalid_encrypted_content")
        || (text.contains("encrypted content")
            && (text.contains("could not be verified") || text.contains("could not be decrypted")))
}

/// 去掉请求里所有加密推理项，并记住它们被 `relay_id` 拒收过。返回去掉了几项。
pub fn strip_and_remember(relay_id: &str, body: &mut Value) -> usize {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut rejected = Vec::new();
    items.retain(|item| match encrypted_content(item) {
        Some(content) => {
            rejected.push(digest(content));
            false
        }
        None => true,
    });
    let mut memory = memory().lock().unwrap_or_else(|error| error.into_inner());
    for hash in &rejected {
        let key = (relay_id.to_string(), *hash);
        if memory.keys.insert(key.clone()) {
            memory.order.push_back(key);
        }
    }
    while memory.order.len() > MEMORY_LIMIT {
        if let Some(oldest) = memory.order.pop_front() {
            memory.keys.remove(&oldest);
        }
    }
    rejected.len()
}

/// 去掉 `relay_id` 之前拒收过的推理项（其余推理项保留）。返回去掉了几项。
pub fn strip_remembered(relay_id: &str, body: &mut Value) -> usize {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let memory = memory().lock().unwrap_or_else(|error| error.into_inner());
    if memory.keys.is_empty() {
        return 0;
    }
    let before = items.len();
    items.retain(|item| {
        encrypted_content(item).is_none_or(|content| {
            !memory
                .keys
                .contains(&(relay_id.to_string(), digest(content)))
        })
    });
    before - items.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_openai_and_wrapped_rejections_only_on_400() {
        let openai = br#"{"error":{"message":"The encrypted content for item rs_1 could not be verified. Reason: Encrypted content item_id did not match the target item id.","type":"invalid_request_error","param":null,"code":"invalid_encrypted_content"}}"#;
        assert!(is_unverifiable_reasoning_rejection(400, openai));
        let wrapped = br#"{"error":{"message":"upstream: The encrypted content gAAA could not be decrypted or parsed."}}"#;
        assert!(is_unverifiable_reasoning_rejection(400, wrapped));
        assert!(!is_unverifiable_reasoning_rejection(500, openai));
        assert!(!is_unverifiable_reasoning_rejection(
            400,
            br#"{"error":{"message":"Unsupported parameter: reasoning.effort"}}"#
        ));
    }

    #[test]
    fn remembered_items_are_stripped_only_for_the_rejecting_relay() {
        let relay = "foreign-reasoning-unit";
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_old", "summary": [], "encrypted_content": "unit-old"},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plain"}]},
        ]});
        assert!(has_encrypted_reasoning(&body));
        assert_eq!(strip_and_remember(relay, &mut body), 1);
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert!(!has_encrypted_reasoning(&body));

        let mut next = json!({"input": [
            {"type": "reasoning", "id": "rs_old", "summary": [], "encrypted_content": "unit-old"},
            {"type": "reasoning", "id": "rs_new", "summary": [], "encrypted_content": "unit-new"},
        ]});
        let mut elsewhere = next.clone();
        assert_eq!(strip_remembered(relay, &mut next), 1);
        assert_eq!(next["input"][0]["id"], "rs_new");
        // 另一家没拒收过：原样保留。
        assert_eq!(strip_remembered("another-relay", &mut elsewhere), 0);
        assert_eq!(elsewhere["input"].as_array().unwrap().len(), 2);
    }
}

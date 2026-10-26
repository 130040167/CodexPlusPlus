//! 网关模式端到端：真实 helper + 模拟上游，覆盖 Claude Code / Grok 经网关的完整链路。
//!
//! 常驻 helper 是进程级单例、状态目录靠进程级环境变量覆盖，所以全部场景放在一个测试里顺序执行。

use std::sync::{Arc, Mutex};

use codex_plus_core::gateway::store::{
    AgentBinding, ClaudeRoleModels, GatewaySettings, GatewayStore, GatewayTarget, GatewayTargetKind,
};
use codex_plus_core::settings::{
    AggregateRelayMember, AggregateRelayProfile, AggregateRelayStrategy, BackendSettings,
    RelayApiKey, RelayProfile, RelayProtocol, SettingsStore,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Captured = Arc<Mutex<Vec<(String, Value)>>>;

/// 起一个模拟上游：按顺序回放 `responses`（用完后重复最后一个），记录每个请求的 (请求头, 请求体)。
async fn mock_upstream(responses: Vec<(u16, &'static str, String)>) -> (String, Captured) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let captured: Captured = Arc::default();
    let sink = captured.clone();
    tokio::spawn(async move {
        let mut index = 0;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (head, body) = read_request(&mut stream).await;
            sink.lock().unwrap().push((head, body));
            let (status, content_type, body) = responses[index.min(responses.len() - 1)].clone();
            index += 1;
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://{address}/v1"), captured)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut buffer = Vec::new();
    let mut chunk = [0; 8192];
    let (header_end, length) = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        break (end + 4, length);
    };
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        buffer.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let body =
        serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap_or(Value::Null);
    (head, body)
}

fn relay(
    id: &str,
    base_url: &str,
    key: &str,
    protocol: RelayProtocol,
    models: &str,
) -> RelayProfile {
    let mut profile = codex_plus_core::provider_import::pool_relay_profile(
        id,
        base_url,
        key,
        protocol,
        models.lines().next().unwrap_or_default(),
        models,
        &[],
    )
    .unwrap();
    profile.id = id.to_string();
    profile
}

fn chat_sse(text: &str) -> String {
    [
        json!({"id":"c1","object":"chat.completion.chunk","model":"up","choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
        json!({"id":"c1","object":"chat.completion.chunk","model":"up","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"c1","object":"chat.completion.chunk","model":"up","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25}}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n"
}

fn chat_tool_sse() -> String {
    [
        json!({"id":"c2","object":"chat.completion.chunk","model":"up","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_7","type":"function","function":{"name":"Bash","arguments":""}}]}}]}),
        json!({"id":"c2","object":"chat.completion.chunk","model":"up","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls\"}"}}]}}]}),
        json!({"id":"c2","object":"chat.completion.chunk","model":"up","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n"
}

fn binding(kind: GatewayTargetKind, id: &str, model: &str, token: &str) -> AgentBinding {
    AgentBinding {
        enabled: true,
        target: GatewayTarget {
            kind,
            id: id.to_string(),
        },
        model: model.to_string(),
        token: token.to_string(),
        claude_roles: ClaudeRoleModels::default(),
        ..AgentBinding::default()
    }
}

fn save(settings: &BackendSettings, gateway: &GatewaySettings) {
    let store = SettingsStore::default();
    let mut settings = settings.clone();
    settings.settings_revision = store.revision().unwrap();
    store.save(&settings).unwrap();
    GatewayStore::default().save(gateway).unwrap();
}

async fn post(port: u16, path: &str, token: &str, body: &Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header("x-api-key", token)
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("x-claude-code-session-id", "session-e2e")
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

fn sse_events(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|block| block.lines().find_map(|line| line.strip_prefix("data: ")))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn claude_request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "stream": stream,
        "system": [{ "type": "text", "text": "You are Claude Code." }],
        "messages": [{ "role": "user", "content": "你好" }],
        "tools": [{ "name": "Bash", "description": "run", "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } } }]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_end_to_end() {
    let state_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var(
            codex_plus_core::paths::STATE_DIR_ENV,
            state_dir.path().to_str().unwrap(),
        );
    }
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    codex_plus_core::resident_helper::ensure_resident_helper(port).unwrap();
    let token = "cpx-gw-claude-e2e";

    // —— 1. Claude → Chat 上游，流式文本；未知模型名改写成主模型 ——
    let (chat_base, chat_seen) =
        mock_upstream(vec![(200, "text/event-stream", chat_sse("你好呀"))]).await;
    let mut settings = BackendSettings {
        relay_profiles: vec![relay(
            "chat",
            &chat_base,
            "sk-chat",
            RelayProtocol::ChatCompletions,
            "deepseek-v4-pro",
        )],
        ..BackendSettings::default()
    };
    let mut gateway = GatewaySettings::default();
    gateway.bindings.insert(
        "claude".into(),
        binding(
            GatewayTargetKind::Provider,
            "chat",
            "deepseek-v4-pro",
            token,
        ),
    );
    save(&settings, &gateway);
    let loaded = SettingsStore::default().load().unwrap();
    assert_eq!(
        loaded.relay_profiles[0].base_url, chat_base,
        "供应商地址落盘后应保留"
    );

    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    let events = sse_events(&text);
    assert_eq!(events.first().unwrap()["type"], "message_start");
    assert_eq!(
        events.first().unwrap()["message"]["model"],
        "claude-sonnet-5"
    );
    assert!(
        events
            .iter()
            .any(|event| event["delta"]["text"] == "你好呀"),
        "{text}"
    );
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    let stop = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(stop["delta"]["stop_reason"], "end_turn");
    {
        let seen = chat_seen.lock().unwrap();
        let (head, body) = &seen[0];
        assert!(head.starts_with("POST /v1/chat/completions"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-chat"),
            "{head}"
        );
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are Claude Code.");
        assert_eq!(body["tools"][0]["function"]["name"], "Bash");
    }
    let log =
        std::fs::read_to_string(state_dir.path().join("request-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("\"agent\":\"claude\""),
        "请求日志应记录 Agent：{log}"
    );

    // —— 2. Chat 上游返回工具调用 → tool_use 块 ——
    let (tool_base, _) = mock_upstream(vec![(200, "text/event-stream", chat_tool_sse())]).await;
    settings.relay_profiles = vec![relay(
        "chat",
        &tool_base,
        "sk-chat",
        RelayProtocol::ChatCompletions,
        "deepseek-v4-pro",
    )];
    save(&settings, &gateway);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    let events = sse_events(&text);
    let start = events
        .iter()
        .find(|event| event["content_block"]["type"] == "tool_use")
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(start["content_block"]["id"], "call_7");
    assert_eq!(start["content_block"]["name"], "Bash");
    let arguments = events
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect::<String>();
    assert_eq!(arguments, "{\"command\":\"ls\"}");
    let stop = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(stop["delta"]["stop_reason"], "tool_use");

    // —— 2b. 上游无视 stream:true 回了整段 JSON：仍然给 Claude Code 完整事件流 ——
    let json_completion = json!({
        "id": "c3", "object": "chat.completion", "model": "up",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "整段回复" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11 }
    })
    .to_string();
    let (json_base, _) = mock_upstream(vec![(200, "application/json", json_completion)]).await;
    settings.relay_profiles = vec![relay(
        "chat",
        &json_base,
        "sk-chat",
        RelayProtocol::ChatCompletions,
        "deepseek-v4-pro",
    )];
    save(&settings, &gateway);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    let events = sse_events(&text);
    assert!(
        events
            .iter()
            .any(|event| event["delta"]["text"] == "整段回复"),
        "{text}"
    );
    assert_eq!(events.last().unwrap()["type"], "message_stop", "{text}");

    // —— 3. 非流式 → 原生 Responses 上游 ——
    let responses_body = json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-up",
        "output": [{ "id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                     "content": [{ "type": "output_text", "text": "来自 Responses", "annotations": [] }] }],
        "usage": { "input_tokens": 30, "input_tokens_details": { "cached_tokens": 10 }, "output_tokens": 4 }
    })
    .to_string();
    let (resp_base, resp_seen) =
        mock_upstream(vec![(200, "application/json", responses_body)]).await;
    settings.relay_profiles = vec![relay(
        "resp",
        &resp_base,
        "sk-resp",
        RelayProtocol::Responses,
        "gpt-up",
    )];
    gateway.bindings.insert(
        "claude".into(),
        binding(GatewayTargetKind::Provider, "resp", "gpt-up", token),
    );
    save(&settings, &gateway);
    let (status, text) = post(
        port,
        "/gw/claude/v1/messages",
        token,
        &claude_request(false),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let message: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"][0]["text"], "来自 Responses");
    assert_eq!(message["usage"]["input_tokens"], 20);
    assert_eq!(message["usage"]["cache_read_input_tokens"], 10);
    {
        let seen = resp_seen.lock().unwrap();
        let (head, body) = &seen[0];
        assert!(head.starts_with("POST /v1/responses"), "{head}");
        assert_eq!(body["model"], "gpt-up");
        assert_eq!(body["instructions"], "You are Claude Code.");
        assert_eq!(body["store"], false);
    }

    // —— 4. Anthropic 上游原样转发 ——
    let anthropic_sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_a\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-up\",\"content\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string();
    let (anth_base, anth_seen) =
        mock_upstream(vec![(200, "text/event-stream", anthropic_sse.clone())]).await;
    settings.relay_profiles = vec![relay(
        "anth",
        &anth_base,
        "sk-anth",
        RelayProtocol::AnthropicMessages,
        "claude-up",
    )];
    gateway.bindings.insert(
        "claude".into(),
        binding(GatewayTargetKind::Provider, "anth", "claude-up", token),
    );
    save(&settings, &gateway);
    let request = claude_request(true);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &request).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(text, anthropic_sse);
    {
        let seen = anth_seen.lock().unwrap();
        let (head, body) = &seen[0];
        let lower = head.to_ascii_lowercase();
        assert!(head.starts_with("POST /v1/messages"), "{head}");
        assert!(lower.contains("x-api-key: sk-anth"), "{head}");
        assert!(
            !lower.contains("cpx-gw-claude"),
            "本地 token 不能发往上游：{head}"
        );
        assert!(
            lower.contains("anthropic-beta: interleaved-thinking-2025-05-14"),
            "{head}"
        );
        let mut expected = request.clone();
        expected["model"] = json!("claude-up");
        assert_eq!(body, &expected, "除模型名外请求体应原样转发");
    }

    // —— 5. 多 Key：第一个 Key 429 → 换第二个，之后的请求直接跳过冷却中的 Key ——
    let (multi_base, multi_seen) = mock_upstream(vec![
        (
            429,
            "application/json",
            "{\"error\":{\"message\":\"slow down\"}}".to_string(),
        ),
        (200, "text/event-stream", chat_sse("第二个 Key")),
        (200, "text/event-stream", chat_sse("还是第二个 Key")),
    ])
    .await;
    let mut multi = relay(
        "multi",
        &multi_base,
        "sk-one",
        RelayProtocol::ChatCompletions,
        "deepseek-v4-pro",
    );
    multi.api_keys = vec![
        RelayApiKey {
            id: "one".into(),
            name: "one".into(),
            api_key: "sk-one".into(),
        },
        RelayApiKey {
            id: "two".into(),
            name: "two".into(),
            api_key: "sk-two".into(),
        },
    ];
    multi.active_api_key_id = "one".into();
    settings.relay_profiles = vec![multi];
    gateway.bindings.insert(
        "claude".into(),
        binding(
            GatewayTargetKind::Provider,
            "multi",
            "deepseek-v4-pro",
            token,
        ),
    );
    save(&settings, &gateway);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("第二个 Key"), "{text}");
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    {
        let seen = multi_seen.lock().unwrap();
        let keys = seen
            .iter()
            .map(|(head, _)| {
                head.lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("authorization: bearer ")
                            .map(str::to_string)
                    })
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["sk-one", "sk-two", "sk-two"]);
    }

    // —— 6. 路由组：400 不切换；5xx 切到下一个成员 ——
    let (bad_base, _) = mock_upstream(vec![
        (
            400,
            "application/json",
            "{\"error\":{\"message\":\"context_length_exceeded\"}}".to_string(),
        ),
        (
            502,
            "application/json",
            "{\"error\":{\"message\":\"bad gateway\"}}".to_string(),
        ),
    ])
    .await;
    let (good_base, good_seen) =
        mock_upstream(vec![(200, "text/event-stream", chat_sse("备用成员"))]).await;
    settings.relay_profiles = vec![
        relay(
            "bad",
            &bad_base,
            "sk-bad",
            RelayProtocol::ChatCompletions,
            "deepseek-v4-pro",
        ),
        relay(
            "good",
            &good_base,
            "sk-good",
            RelayProtocol::ChatCompletions,
            "glm-5.3",
        ),
    ];
    settings.aggregate_relay_profiles = vec![AggregateRelayProfile {
        id: "group".into(),
        name: "group".into(),
        session_provider: Default::default(),
        model_aggregation_enabled: false,
        strategy: AggregateRelayStrategy::Failover,
        members: vec![
            AggregateRelayMember {
                relay_id: "bad".into(),
                weight: 1,
            },
            AggregateRelayMember {
                relay_id: "good".into(),
                weight: 1,
            },
        ],
        routes: Vec::new(),
        default_relay_id: String::new(),
        sync_default_member_login: false,
    }];
    gateway.bindings.insert(
        "claude".into(),
        binding(GatewayTargetKind::Group, "group", "deepseek-v4-pro", token),
    );
    save(&settings, &gateway);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 400, "{text}");
    let error: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(good_seen.lock().unwrap().is_empty(), "400 不应切到其它成员");
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("备用成员"), "{text}");
    assert_eq!(
        good_seen.lock().unwrap()[0].1["model"],
        "glm-5.3",
        "成员不认识的模型换成成员默认模型"
    );

    // —— 7. 鉴权与开关 ——
    let (status, text) = post(
        port,
        "/gw/claude/v1/messages",
        "cpx-gw-claude-wrong",
        &claude_request(true),
    )
    .await;
    assert_eq!(status, 401, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["error"]["type"],
        "authentication_error"
    );
    let (status, text) = post(
        port,
        "/gw/claude/v1/messages/count_tokens",
        token,
        &claude_request(false),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(
        serde_json::from_str::<Value>(&text).unwrap()["input_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    let models = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/gw/claude/v1/models"))
        .header("x-api-key", token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "deepseek-v4-pro");

    // —— 8. Grok：Responses 客户端 → Chat 上游，输出 Responses 事件流 ——
    let (grok_base, grok_seen) =
        mock_upstream(vec![(200, "text/event-stream", chat_sse("Grok 你好"))]).await;
    settings.relay_profiles = vec![relay(
        "grokup",
        &grok_base,
        "sk-grok",
        RelayProtocol::ChatCompletions,
        "glm-5.3",
    )];
    gateway.bindings.insert(
        "grok".into(),
        binding(
            GatewayTargetKind::Provider,
            "grokup",
            "glm-5.3",
            "cpx-gw-grok-e2e",
        ),
    );
    save(&settings, &gateway);
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/gw/grok/v1/responses"))
        .bearer_auth("cpx-gw-grok-e2e")
        .json(&json!({
            "model": "glm-5.3",
            "stream": true,
            "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("response.output_text.delta"), "{text}");
    assert!(text.contains("Grok 你好"), "{text}");
    assert!(text.contains("response.completed"), "{text}");
    assert_eq!(grok_seen.lock().unwrap()[0].1["model"], "glm-5.3");

    // —— 9. 关闭绑定后返回 503，旧的 Codex 代理路径不受影响 ——
    gateway.bindings.get_mut("claude").unwrap().enabled = false;
    save(&settings, &gateway);
    let (status, text) = post(port, "/gw/claude/v1/messages", token, &claude_request(true)).await;
    assert_eq!(status, 503, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["type"],
        "error"
    );

    codex_plus_core::resident_helper::stop_resident_helper();
}

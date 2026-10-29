//! 别家签发的加密推理（#2777）：同一对话里先用中转、再换到官方账号（或反过来）时，
//! 新上游校验不过历史里的 `encrypted_content`，会整条请求 400。
//!
//! 守住三条：报了这个错时去掉加密推理项重发一次并成功，之后同一批旧推理项提前去掉；
//! 正常请求原样转发、不碰 reasoning 项；别的 400 不重试。

use codex_plus_core::protocol_proxy::{
    ForcedRelay, ProxySessionHeaders, open_responses_proxy_request_for_relay,
    open_responses_proxy_request_with_settings_for_path,
};
use codex_plus_core::settings::{
    BackendSettings, RelayMode, RelayProfile, RelayProtocol, SettingsStore,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// 中转签发、官方校验不过的推理内容。
const FOREIGN: &str = "relay-signed-reasoning";

/// 每个用例用自己的供应商 id：被拒推理项按「供应商 + 内容」记在进程里。
fn relay(server: &MockServer, label: &str) -> RelayProfile {
    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);
    let id = format!(
        "foreign-reasoning-{label}-{}",
        FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
    );
    RelayProfile {
        id: id.clone(),
        name: id,
        relay_mode: RelayMode::PureApi,
        protocol: RelayProtocol::Responses,
        base_url: format!("{}/v1", server.uri()),
        model: "gpt-5.5".to_string(),
        model_list: "gpt-5.5".to_string(),
        api_key: "local-fixture-key".to_string(),
        ..RelayProfile::default()
    }
}

fn reasoning(id: &str, content: &str) -> Value {
    json!({"type": "reasoning", "id": id, "summary": [], "encrypted_content": content})
}

fn request(input: Vec<Value>) -> Value {
    json!({"model": "gpt-5.5", "stream": false, "store": false, "input": input})
}

fn completed() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "resp_fixture", "object": "response", "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    }))
}

/// OpenAI 对别家加密推理的原样报错。
fn unverifiable() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": {
        "message": "The encrypted content for item rs_relay could not be verified. Reason: Encrypted content item_id did not match the target item id.",
        "type": "invalid_request_error", "param": null, "code": "invalid_encrypted_content"
    }}))
}

/// 模拟官方：只要请求里还带着中转签发的推理就拒收。
async fn official_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|incoming: &Request| {
            if String::from_utf8_lossy(&incoming.body).contains(FOREIGN) {
                unverifiable()
            } else {
                completed()
            }
        })
        .mount(&server)
        .await;
    server
}

async fn send_forced(relay: &RelayProfile, body: &Value) -> (u16, Vec<u8>) {
    let mut response = open_responses_proxy_request_for_relay(
        &body.to_string(),
        BackendSettings::default(),
        ForcedRelay {
            relay: relay.clone(),
            upstream_model: None,
            auto_retry: false,
        },
        None,
        "/responses",
        ProxySessionHeaders::default(),
        None,
    )
    .await
    .unwrap();
    let status = response.status_code;
    (status, response.read_body().await.unwrap())
}

async fn received(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.body_json::<Value>().unwrap())
        .collect()
}

#[tokio::test]
async fn unverifiable_foreign_reasoning_is_stripped_and_retried_once() {
    let server = official_upstream().await;
    let relay = relay(&server, "gateway");
    let user = json!({"type": "message", "role": "user", "content": "继续"});
    let call =
        json!({"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{}"});
    let output = json!({"type": "function_call_output", "call_id": "call_1", "output": "done"});
    let first = request(vec![
        user.clone(),
        reasoning("rs_relay", FOREIGN),
        call.clone(),
        output.clone(),
    ]);

    let (status, body) = send_forced(&relay, &first).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let sent = received(&server).await;
    assert_eq!(sent.len(), 2, "只重发一次");
    assert_eq!(sent[0], first, "第一次原样转发");
    // 重发只去掉加密推理项，消息与工具调用历史原样保留。
    assert_eq!(sent[1]["input"], json!([user, call, output]));

    // 下一轮：旧的中转推理还在历史里，官方新签发的推理也在。旧的提前去掉，不再先吃一次 400。
    let second = request(vec![
        user.clone(),
        reasoning("rs_relay", FOREIGN),
        reasoning("rs_official", "official-signed-reasoning"),
        json!({"type": "message", "role": "user", "content": "再来"}),
    ]);
    let (status, _) = send_forced(&relay, &second).await;
    assert_eq!(status, 200);
    let sent = received(&server).await;
    assert_eq!(sent.len(), 3, "已知被拒的推理项提前去掉，一次就成功");
    let kept: Vec<&Value> = sent[2]["input"].as_array().unwrap().iter().collect();
    assert_eq!(kept.len(), 3);
    assert_eq!(kept[1]["id"], "rs_official", "官方自己签发的推理保留");
}

#[tokio::test]
async fn ordinary_requests_keep_reasoning_items_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(completed())
        .mount(&server)
        .await;
    let relay = relay(&server, "ordinary");
    let body = request(vec![
        json!({"type": "message", "role": "user", "content": "hi"}),
        reasoning("rs_same", "same-provider-reasoning"),
        json!({"type": "message", "role": "user", "content": "again"}),
    ]);

    let (status, _) = send_forced(&relay, &body).await;
    assert_eq!(status, 200);
    let sent = received(&server).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0], body, "没切换供应商的请求不改动 reasoning 项");
}

#[tokio::test]
async fn other_bad_requests_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Unsupported parameter: 'temperature'", "type": "invalid_request_error",
            "param": "temperature", "code": "unsupported_parameter"
        }})))
        .mount(&server)
        .await;
    let relay = relay(&server, "other-400");
    let body = request(vec![
        json!({"type": "message", "role": "user", "content": "hi"}),
        reasoning("rs_keep", "kept-reasoning"),
    ]);

    let (status, error) = send_forced(&relay, &body).await;
    assert_eq!(status, 400);
    let error: Value = serde_json::from_slice(&error).unwrap();
    assert_eq!(
        error["error"]["code"], "unsupported_parameter",
        "错误体原样交回"
    );
    let sent = received(&server).await;
    assert_eq!(sent.len(), 1, "别的 400 不重试");
    assert_eq!(sent[0], body);
}

#[tokio::test]
async fn direct_local_proxy_path_also_retries_without_foreign_reasoning() {
    // 直连模式经本地协议代理（聚合、需要转换的供应商）走的是同一条链路。
    let _guard = crate::common::global_state_lock();
    let server = official_upstream().await;
    let relay = relay(&server, "direct");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let settings = BackendSettings {
        active_relay_id: relay.id.clone(),
        relay_profiles: vec![relay],
        protocol_proxy_auto_retry: false,
        ..BackendSettings::default()
    };
    SettingsStore::new(path.clone()).save(&settings).unwrap();
    let previous = codex_plus_core::paths::set_settings_path_for_tests(Some(path));
    let body = request(vec![
        json!({"type": "message", "role": "user", "content": "hi"}),
        reasoning("rs_relay", FOREIGN),
    ]);

    let response = open_responses_proxy_request_with_settings_for_path(
        &body.to_string(),
        settings,
        "/v1/responses",
    )
    .await;
    codex_plus_core::paths::set_settings_path_for_tests(previous);
    let response = response.unwrap();
    assert_eq!(response.status_code, 200);
    let trace = response.request_trace.expect("请求日志轨迹");
    assert_eq!(
        trace.retry_reasons,
        vec!["invalid_encrypted_content".to_string()]
    );
    let sent = received(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[1]["input"],
        json!([{"type": "message", "role": "user", "content": "hi"}])
    );
}

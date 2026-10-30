use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalRequest {
    host_id: String,
    thread_id: String,
    request_id: String,
    params: GoalParams,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalParams {
    objective: String,
    append_transcript_item: Option<bool>,
    thread_settings: Option<Value>,
}

fn validate_request(payload: &Value) -> anyhow::Result<()> {
    let request: GoalRequest = serde_json::from_value(payload.clone())?;
    for value in [&request.host_id, &request.thread_id, &request.request_id] {
        anyhow::ensure!(!value.trim().is_empty() && value.len() <= 256, "无效的会话目标标识");
    }
    anyhow::ensure!(
        !request.params.objective.trim().is_empty() && request.params.objective.len() <= 100_000,
        "目标内容不能为空或过长"
    );
    anyhow::ensure!(
        request.params.thread_settings.as_ref().is_none_or(|value| value.is_object() || value.is_null()),
        "无效的会话设置"
    );
    let _ = request.params.append_transcript_item;
    Ok(())
}

fn is_app_window(target: &crate::cdp::CdpTarget) -> bool {
    target.target_type == "page"
        && target.web_socket_debugger_url.is_some()
        && reqwest::Url::parse(&target.url).is_ok_and(|url| {
            url.scheme() == "app" && url.host_str() == Some("-") && url.path() == "/index.html"
        })
}

async fn evaluate(websocket: &str, method: &str, payload: &Value) -> anyhow::Result<Value> {
    // 原始 Runtime.evaluate 默认不序列化对象；异步完成后显式编码，不能把 Promise 当成功。
    let script = format!(
        "{}\n;(async()=>JSON.stringify(await window.__codexPlusNativeThreadGoal.{}({})))()",
        crate::assets::NATIVE_THREAD_GOAL_SCRIPT, method, payload,
    );
    let timeout = std::time::Duration::from_secs(if method == "execute" { 20 } else { 3 });
    let response = crate::bridge::evaluate_script_with_timeout(websocket, &script, true, timeout).await?;
    anyhow::ensure!(
        response.pointer("/result/exceptionDetails").is_none(),
        "会话所在窗口未能完成目标操作"
    );
    let text = response.pointer("/result/result/value").and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("会话所在窗口未返回目标操作结果"))?;
    Ok(serde_json::from_str(text)?)
}

/// 只在指定 Codex 实例的唯一原生 owner 窗口中设置目标。
pub async fn set_in_owner(debug_port: u16, payload: Value) -> anyhow::Result<Value> {
    validate_request(&payload)?;
    let probe = json!({ "hostId": payload["hostId"], "threadId": payload["threadId"] });
    let targets = crate::cdp::list_targets(debug_port).await?;
    let mut owners = Vec::new();
    let candidates: Vec<_> = targets.iter().filter(|target| is_app_window(target))
        .filter_map(|target| target.web_socket_debugger_url.as_deref()).collect();
    let results = futures_util::future::join_all(candidates.iter().map(|websocket| {
        evaluate(websocket, "probe", &probe)
    })).await;
    for (websocket, result) in candidates.into_iter().zip(results) {
        match result {
            Ok(result) if result["status"] == "owner" => owners.push(websocket),
            Ok(_) => {}
            Err(_) => anyhow::bail!("暂时无法确认会话窗口状态，请稍后重试"),
        }
    }
    anyhow::ensure!(owners.len() == 1, "未找到唯一的会话所属窗口，请打开原窗口后重试");
    // CDP 超时不会取消已经执行的 JS；写入只发给这个 owner 一次，禁止改投其他窗口。
    evaluate(owners[0], "execute", &payload).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{
        Message, handshake::derive_accept_key, protocol::Role,
    };

    #[derive(Clone)]
    enum FixtureReply {
        Json(Value),
        Disconnect,
        Exception,
    }

    struct FixtureWindow {
        id: String,
        url: &'static str,
        probe: FixtureReply,
        execute: FixtureReply,
    }

    fn window(id: &str, status: &str) -> FixtureWindow {
        FixtureWindow {
            id: id.to_string(),
            url: "app://-/index.html",
            probe: FixtureReply::Json(json!({ "status": status })),
            execute: FixtureReply::Json(json!({ "ok": true })),
        }
    }

    #[derive(Clone, Debug)]
    struct ObservedCommand {
        target: String,
        operation: String,
        payload: Value,
        params: Value,
    }

    struct FakeCdp {
        port: u16,
        commands: Arc<Mutex<Vec<ObservedCommand>>>,
        task: Option<tokio::task::JoinHandle<()>>,
    }

    impl FakeCdp {
        async fn start(windows: Vec<FixtureWindow>) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let targets = Value::Array(windows.iter().map(|window| json!({
                "id": window.id,
                "type": "page",
                "title": "",
                "url": window.url,
                "webSocketDebuggerUrl": format!(
                    "ws://127.0.0.1:{port}/devtools/page/{}", window.id
                ),
            })).collect()).to_string();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::clone(&commands);
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                        assert!(header.len() <= 16_384);
                    }
                    let header = String::from_utf8(header).unwrap();
                    let path = header.lines().next().unwrap()
                        .split_whitespace().nth(1).unwrap();
                    if path == "/json" {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{targets}",
                            targets.len(),
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                        continue;
                    }
                    let target = path.strip_prefix("/devtools/page/").unwrap();
                    let window = windows.iter().find(|window| window.id == target).unwrap();
                    let key = header.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
                    }).unwrap();
                    let response = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                        derive_accept_key(key.as_bytes()),
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                        stream, Role::Server, None,
                    ).await;
                    let message = socket.next().await.unwrap().unwrap();
                    let command: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                    assert_eq!(command["method"], "Runtime.evaluate");
                    let expression = command["params"]["expression"].as_str().unwrap();
                    let call = expression.rsplit_once(
                        "await window.__codexPlusNativeThreadGoal."
                    ).unwrap().1;
                    let (operation, arguments) = call.split_once('(').unwrap();
                    let payload = serde_json::from_str(
                        arguments.strip_suffix(")))()").unwrap()
                    ).unwrap();
                    observed.lock().unwrap().push(ObservedCommand {
                        target: target.to_string(),
                        operation: operation.to_string(),
                        payload,
                        params: command["params"].clone(),
                    });
                    let reply = match operation {
                        "probe" => window.probe.clone(),
                        "execute" => window.execute.clone(),
                        _ => panic!("unexpected operation: {operation}"),
                    };
                    let result = match reply {
                        FixtureReply::Json(value) => json!({
                            "result": {
                                "type": "string",
                                "value": serde_json::to_string(&value).unwrap(),
                            }
                        }),
                        FixtureReply::Disconnect => continue,
                        FixtureReply::Exception => json!({
                            "result": { "type": "undefined" },
                            "exceptionDetails": { "text": "native setter rejected" },
                        }),
                    };
                    let response = json!({ "id": command["id"], "result": result });
                    socket.send(Message::Text(response.to_string().into())).await.unwrap();
                }
            });
            Self { port, commands, task: Some(task) }
        }

        fn commands(&self) -> Vec<ObservedCommand> {
            self.commands.lock().unwrap().clone()
        }

        fn websocket(&self, target: &str) -> String {
            format!("ws://127.0.0.1:{}/devtools/page/{target}", self.port)
        }

        async fn stop(mut self) {
            let task = self.task.take().unwrap();
            task.abort();
            let error = task.await.unwrap_err();
            assert!(error.is_cancelled(), "fixture failed: {error}");
        }
    }

    impl Drop for FakeCdp {
        fn drop(&mut self) {
            if let Some(task) = &self.task {
                task.abort();
            }
        }
    }

    fn goal_payload() -> Value {
        json!({
            "hostId": "local",
            "threadId": "thread",
            "requestId": "request",
            "params": {
                "objective": "test \"goal\"\nsecond line",
                "appendTranscriptItem": false,
                "threadSettings": { "model": "fixture-model" },
            },
        })
    }

    #[tokio::test]
    async fn no_owner_never_executes() {
        let fixture = FakeCdp::start(vec![
            window("first", "follower"),
            window("second", "missing"),
        ]).await;
        let error = set_in_owner(fixture.port, goal_payload()).await.unwrap_err();
        assert!(error.to_string().contains("未找到唯一"));
        let commands = fixture.commands();
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().all(|command| command.operation == "probe"));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn multiple_owners_never_execute() {
        let fixture = FakeCdp::start(vec![
            window("first", "owner"),
            window("second", "owner"),
        ]).await;
        let error = set_in_owner(fixture.port, goal_payload()).await.unwrap_err();
        assert!(error.to_string().contains("未找到唯一"));
        let commands = fixture.commands();
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().all(|command| command.operation == "probe"));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn unique_hidden_spotlight_owner_executes_once_with_original_payload() {
        let mut spotlight = window("spotlight", "owner");
        spotlight.url = "app://-/index.html?initialRoute=%2Fspotlight";
        let mut external = window("external", "owner");
        external.url = "https://codex.example/index.html";
        let fixture = FakeCdp::start(vec![
            window("main", "follower"), spotlight, external,
        ]).await;
        let payload = goal_payload();
        assert_eq!(
            set_in_owner(fixture.port, payload.clone()).await.unwrap(),
            json!({ "ok": true }),
        );
        let commands = fixture.commands();
        assert_eq!(commands.len(), 3);
        assert!(commands.iter().all(|command| command.target != "external"));
        for command in commands.iter().filter(|command| command.operation == "probe") {
            assert_eq!(command.payload, json!({ "hostId": "local", "threadId": "thread" }));
        }
        let executed: Vec<_> = commands.iter()
            .filter(|command| command.operation == "execute").collect();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].target, "spotlight");
        assert_eq!(executed[0].payload, payload);
        assert!(commands.iter().all(|command| command.params["awaitPromise"] == true));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn probe_disconnect_prevents_all_execution() {
        let mut unreachable = window("unreachable", "follower");
        unreachable.probe = FixtureReply::Disconnect;
        let fixture = FakeCdp::start(vec![
            window("owner", "owner"), unreachable,
        ]).await;
        let error = set_in_owner(fixture.port, goal_payload()).await.unwrap_err();
        assert!(error.to_string().contains("暂时无法确认"));
        let commands = fixture.commands();
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().all(|command| command.operation == "probe"));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn awaited_json_string_results_preserve_primitives_and_structured_values() {
        let values = [
            Value::Null,
            json!(true),
            json!(42),
            json!("quoted \"result\"\nsecond line"),
            json!([1, null, "result"]),
            json!({ "status": "owner", "nested": { "ok": true } }),
        ];
        let windows = values.iter().enumerate().map(|(index, value)| {
            let mut window = window(&index.to_string(), "owner");
            window.execute = FixtureReply::Json(value.clone());
            window
        }).collect();
        let fixture = FakeCdp::start(windows).await;
        for (index, expected) in values.iter().enumerate() {
            let actual = evaluate(
                &fixture.websocket(&index.to_string()), "execute", &goal_payload(),
            ).await.unwrap();
            assert_eq!(&actual, expected);
        }
        let commands = fixture.commands();
        assert_eq!(commands.len(), values.len());
        assert!(commands.iter().all(|command| command.params["awaitPromise"] == true));
        fixture.stop().await;
    }

    #[tokio::test]
    async fn failed_execution_never_retries_or_moves_to_another_window() {
        for failure in [FixtureReply::Disconnect, FixtureReply::Exception] {
            let mut owner = window("owner", "owner");
            owner.execute = failure;
            let fixture = FakeCdp::start(vec![
                window("follower", "follower"), owner,
            ]).await;
            assert!(set_in_owner(fixture.port, goal_payload()).await.is_err());
            let commands = fixture.commands();
            assert_eq!(commands.len(), 3);
            let executed: Vec<_> = commands.iter()
                .filter(|command| command.operation == "execute").collect();
            assert_eq!(executed.len(), 1);
            assert_eq!(executed[0].target, "owner");
            fixture.stop().await;
        }
    }

    #[test]
    fn accepts_only_goal_parameters() {
        let payload = json!({"hostId":"local","threadId":"thread","requestId":"request",
            "params":{"objective":"test","appendTranscriptItem":false,"threadSettings":null}});
        assert!(validate_request(&payload).is_ok());
        for invalid in [
            json!({"params":{"objective":""}}),
            json!({"hostId":"local","threadId":"thread","requestId":"request",
                "params":{"objective":"test","threadSettings":[]}}),
        ] {
            assert!(validate_request(&invalid).is_err());
        }
        let mut arbitrary = payload.clone();
        arbitrary["expression"] = json!("anything");
        assert!(validate_request(&arbitrary).is_err());
    }

    #[test]
    fn filters_actual_app_windows_including_spotlight() {
        for (url, expected) in [
            ("app://-/index.html", true),
            ("app://-/index.html?initialRoute=%2Fspotlight", true),
            ("https://codex.example/index.html", false),
            ("app://other/index.html", false),
            ("app://-/browser.html", false),
        ] {
            let target: crate::cdp::CdpTarget = serde_json::from_value(json!({
                "id":"test","type":"page","title":"Codex","url":url,
                "webSocketDebuggerUrl":"ws://127.0.0.1/test"
            })).unwrap();
            assert_eq!(is_app_window(&target), expected, "{url}");
        }
    }
}

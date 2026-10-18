use codex_plus_core::protocol_proxy::open_responses_proxy_request_with_settings;
use codex_plus_core::settings::{BackendSettings, RelayMode, RelayProfile, RelayProtocol};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

fn search_history() -> Value {
    json!([
        {
            "type": "tool_search_call", "id": "tsc_first", "call_id": "first",
            "execution": "client", "status": "completed", "arguments": {"query": "search"}
        },
        {
            "type": "tool_search_output", "id": "tso_first", "call_id": "first",
            "execution": "client", "status": "completed",
            "tools": [{
                "type": "namespace", "name": "repo", "description": "old",
                "tools": [{
                    "type": "function", "name": "search",
                    "parameters": {"type": "object"}
                }]
            }]
        },
        {
            "type": "function_call", "id": "fc_read", "call_id": "read",
            "namespace": "repo", "name": "search", "arguments": "{}"
        },
        {"type": "function_call_output", "call_id": "read", "output": "fixture result"},
        {
            "type": "tool_search_call", "id": "tsc_second", "call_id": "second",
            "execution": "client", "status": "completed", "arguments": {"query": "read"}
        },
        {
            "type": "tool_search_output", "id": "tso_second", "call_id": "second",
            "execution": "client", "status": "completed",
            "tools": [{
                "type": "namespace", "name": "repo", "description": "latest",
                "tools": [
                    {
                        "type": "function", "name": "search",
                        "parameters": {
                            "type": "object", "properties": {"query": {"type": "string"}}
                        }
                    },
                    {"type": "function", "name": "read", "parameters": {"type": "object"}}
                ]
            }]
        },
        {
            "type": "tool_search_call", "id": "tsc_empty", "call_id": "empty",
            "execution": "client", "status": "completed", "arguments": {"query": "absent"}
        },
        {
            "type": "tool_search_output", "id": "tso_empty", "call_id": "empty",
            "execution": "client", "status": "completed", "tools": []
        },
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
    ])
}

#[tokio::test]
async fn responses_forwarding_preserves_search_pairs_and_unique_namespaces() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let items = body["input"].as_array().unwrap();
            for call in items.iter().filter(|item| item["type"] == "tool_search_call") {
                if !items.iter().any(|output| {
                    output["type"] == "tool_search_output" && output["call_id"] == call["call_id"]
                }) {
                    return ResponseTemplate::new(400).set_body_json(json!({
                        "error": {
                            "type": "invalid_request_error",
                            "message": format!("No tool output found for tool search call {}.", call["id"].as_str().unwrap())
                        }
                    }));
                }
            }
            let mut namespaces = std::collections::HashSet::new();
            for item in items.iter().filter(|item| item["type"] == "tool_search_output") {
                for tool in item["tools"].as_array().unwrap() {
                    if tool["type"] == "namespace"
                        && !namespaces.insert(tool["name"].as_str().unwrap())
                    {
                        return ResponseTemplate::new(400).set_body_json(json!({
                            "error": {"type": "invalid_request_error", "message": "Duplicate namespace"}
                        }));
                    }
                }
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp_fixture", "object": "response", "status": "completed", "output": []
            }))
        })
        .mount(&upstream)
        .await;
    let settings = BackendSettings {
        relay_profiles_enabled: true,
        active_relay_id: "fixture".into(),
        relay_profiles: vec![RelayProfile {
            id: "fixture".into(),
            base_url: format!("{}/v1", upstream.uri()),
            api_key: "fixture-only".into(),
            protocol: RelayProtocol::Responses,
            relay_mode: RelayMode::PureApi,
            ..RelayProfile::default()
        }],
        ..BackendSettings::default()
    };
    let original = search_history();
    for stream in [false, true] {
        let request = json!({
            "model": "fixture", "stream": stream,
            "tools": [{"type": "tool_search", "execution": "client"}],
            "input": original
        });
        let mut response =
            open_responses_proxy_request_with_settings(&request.to_string(), settings.clone())
                .await
                .unwrap();
        let payload = response.read_body().await.unwrap();
        assert_eq!(
            response.status_code,
            200,
            "strict upstream rejected forwarded history: {}",
            String::from_utf8_lossy(&payload)
        );
    }
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let sent: Value = serde_json::from_slice(&request.body).unwrap();
        let items = sent["input"].as_array().unwrap();
        assert_eq!(items.len(), original.as_array().unwrap().len());
        for (sent, original) in items.iter().zip(original.as_array().unwrap()) {
            let mut envelope = sent.clone();
            let mut expected = original.clone();
            if sent["type"] == "tool_search_output" {
                envelope.as_object_mut().unwrap().remove("tools");
                expected.as_object_mut().unwrap().remove("tools");
            }
            assert_eq!(
                envelope, expected,
                "history order and call/output envelopes must survive"
            );
        }
        let namespaces: Vec<_> = items
            .iter()
            .filter(|item| item["type"] == "tool_search_output")
            .flat_map(|item| item["tools"].as_array().unwrap())
            .collect();
        assert_eq!(namespaces.len(), 1);
        assert_eq!(namespaces[0]["description"], "latest");
        let tools = namespaces[0]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0]["parameters"]["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(tools[1]["name"], "read");
    }
}

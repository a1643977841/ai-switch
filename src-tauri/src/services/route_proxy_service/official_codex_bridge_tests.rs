use super::*;

fn official_credential(config: Value) -> SelectedCredential {
    SelectedCredential {
        id: "official-chat-bridge".to_string(),
        platform: "codex".to_string(),
        kind: "official".to_string(),
        display_name: "Official chat bridge".to_string(),
        status: "ok".to_string(),
        route_priority: 3,
        max_concurrency: 1,
        secret_payload_json: json!({
            "access_token": "at-official-test",
            "account_id": "acct-official-test"
        })
        .to_string(),
        config_json: config.to_string(),
    }
}

fn chat_request(streaming: bool) -> Value {
    json!({
        "model": "friendly-codex",
        "messages": [
            {"role": "system", "content": "Use tools when needed."},
            {"role": "user", "content": "hello"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "Look up a value",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
            }
        }],
        "max_tokens": 512,
        "stream": streaming
    })
}

#[test]
fn official_codex_chat_request_bridges_before_backend_normalization() {
    let credential = official_credential(json!({
        "model_mappings": [{"from": "friendly-codex", "to": "gpt-5.6-sol"}]
    }));
    for path in [
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/chat/completions/",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("application/json"));
        let built = build_upstream_request_with_bridge(
            &credential,
            "codex",
            path,
            Some("trace=test"),
            headers,
            &serde_json::to_vec(&chat_request(true)).unwrap(),
            TurnReminderMode::Apply,
        )
        .expect("official chat request");
        assert_eq!(
            built.target_url,
            "https://chatgpt.com/backend-api/codex/responses?trace=test"
        );
        assert_eq!(built.bridge_kind, Some(ProtocolBridgeKind::ChatToResponses));
        assert!(built.streaming_request);
        assert_eq!(built.headers["authorization"], "Bearer at-official-test");
        assert_eq!(built.headers["chatgpt-account-id"], "acct-official-test");
        assert_eq!(built.headers["accept"], "text/event-stream");
        let body: Value = serde_json::from_slice(&built.body).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["instructions"], "Use tools when needed.");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["store"], false);
        assert!(body.get("messages").is_none());
        // Chat max_tokens becomes max_output_tokens during bridging. The private
        // backend rejects it, so normalization must happen AFTER conversion.
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("max_tokens").is_none());
    }
}

#[test]
fn official_codex_chat_relay_preserves_public_responses_limits() {
    let credential = official_credential(json!({"base_url": "https://relay.example/v1"}));
    let built = build_upstream_request_with_bridge(
        &credential,
        "codex",
        "/v1/chat/completions",
        None,
        HeaderMap::new(),
        &serde_json::to_vec(&chat_request(false)).unwrap(),
        TurnReminderMode::Apply,
    )
    .unwrap();
    assert_eq!(built.target_url, "https://relay.example/v1/responses");
    assert_eq!(built.bridge_kind, Some(ProtocolBridgeKind::ChatToResponses));
    assert!(!built.streaming_request);
    assert!(built.headers.get("chatgpt-account-id").is_none());
    let body: Value = serde_json::from_slice(&built.body).unwrap();
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(body["stream"], false);
}

async fn proxy_official_request(
    path: &str,
    request: Value,
    bridge_name: &str,
) -> (String, axum::body::Bytes) {
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/backend-api/codex/responses",
        axum::routing::post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let sender = sender.clone();
            async move {
                sender.send((headers, body.clone())).unwrap();
                if body.get("messages").is_some()
                    || body.get("input").is_none()
                    || body["store"] != false
                    || body.get("max_output_tokens").is_some()
                {
                    return (StatusCode::BAD_REQUEST, "invalid private Responses request")
                        .into_response();
                }
                let response = json!({
                    "id": "resp-official-test", "object": "response", "status": "completed",
                    "model": "gpt-5.6-sol",
                    "output": [
                        {"type": "message", "role": "assistant", "content": [
                            {"type": "output_text", "text": "hello"}
                        ]},
                        {"type": "function_call", "id": "fc-1", "call_id": "call-1",
                         "name": "lookup", "arguments": "{\"q\":\"test\"}"}
                    ],
                    "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
                });
                if body["stream"] == true {
                    let created = json!({"type": "response.created", "response": {
                        "id": "resp-official-test", "model": "gpt-5.6-sol", "status": "in_progress"
                    }});
                    let delta = json!({"type": "response.output_text.delta", "delta": "hello"});
                    let tool_done = json!({"type": "response.output_item.done", "output_index": 1,
                        "item": response["output"][1]});
                    let completed = json!({"type": "response.completed", "response": response});
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        format!("event: response.created\ndata: {created}\n\nevent: response.output_text.delta\ndata: {delta}\n\nevent: response.output_item.done\ndata: {tool_done}\n\nevent: response.completed\ndata: {completed}\n\n"),
                    )
                        .into_response()
                } else {
                    axum::Json(response).into_response()
                }
            }
        }),
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let credential = official_credential(json!({
        "base_url": format!("http://{address}/backend-api/codex"),
        "model_mappings": [{"from": "friendly-codex", "to": "gpt-5.6-sol"}],
        "failure_policy": {"retry_count": 0}
    }));
    let credential = RouteCredentialRepository::create(
        &pool,
        "codex",
        "official",
        &credential.display_name,
        None,
        "ok",
        None,
        &credential.secret_payload_json,
        &credential.config_json,
        "{}",
    )
    .await
    .unwrap();
    RoutePoolRepository::replace_members(&pool, "codex", &[credential.id])
        .await
        .unwrap();
    let key = RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-test-official-chat")
        .await
        .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool, &runtime);
    let log = state.live_log.clone();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
    );
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        path.parse().unwrap(),
        Body::from(request.to_string()),
    )
    .await;
    let status = response.status();
    let content_type = response.headers()[axum::http::header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let (headers, upstream) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer at-official-test");
    assert_eq!(headers["chatgpt-account-id"], "acct-official-test");
    assert_eq!(upstream["model"], "gpt-5.6-sol");
    assert_eq!(upstream["input"][0]["content"][0]["text"], "hello");
    assert_eq!(upstream["tools"][0]["name"], "lookup");
    let entries = log.snapshot();
    let entry = entries.last().unwrap();
    assert!(entry.success);
    assert_eq!(entry.bridge.as_deref(), Some(bridge_name));
    assert_eq!(
        entry.target_url.as_deref(),
        Some(format!("http://{address}/backend-api/codex/responses").as_str())
    );
    server.abort();
    (content_type, bytes)
}

async fn exercise_official_codex_chat_proxy(streaming: bool) {
    let (content_type, bytes) = proxy_official_request(
        "/v1/chat/completions",
        chat_request(streaming),
        "ChatToResponses",
    )
    .await;
    if streaming {
        assert!(content_type.starts_with("text/event-stream"));
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("data: [DONE]"));
        assert!(!text.contains("event: response."));
        let chunks: Vec<Value> = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data: ")
                    .and_then(|data| serde_json::from_str(data).ok())
            })
            .collect();
        assert!(chunks
            .iter()
            .all(|chunk| chunk["object"] == "chat.completion.chunk"));
        assert!(chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["delta"]["content"] == "hello"));
        assert!(chunks
            .iter()
            .any(
                |chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
                    == "lookup"
            ));
        assert!(chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["finish_reason"] == "tool_calls"));
        assert!(chunks
            .iter()
            .any(|chunk| chunk["usage"]["prompt_tokens"] == 3));
    } else {
        assert!(content_type.starts_with("application/json"));
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            "call-1"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"test\"}"
        );
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(body["usage"]["prompt_tokens"], 3);
        assert_eq!(body["usage"]["completion_tokens"], 2);
    }
}

#[tokio::test]
async fn official_codex_chat_proxy_returns_chat_json() {
    exercise_official_codex_chat_proxy(false).await;
}

#[tokio::test]
async fn official_codex_chat_proxy_returns_chat_sse() {
    exercise_official_codex_chat_proxy(true).await;
}

fn messages_request(streaming: bool) -> Value {
    json!({
        "model": "friendly-codex",
        "system": [{"type": "text", "text": "Use tools when needed."}],
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "call-previous", "name": "lookup", "input": {"q": "previous"}
            }]},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "call-previous", "content": "previous-result"
            }]}
        ],
        "tools": [{
            "name": "lookup", "description": "Look up a value",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}
        }],
        "tool_choice": {"type": "auto"},
        "max_tokens": 512,
        "stream": streaming
    })
}

#[test]
fn official_codex_messages_request_bridges_before_backend_normalization() {
    let credential = official_credential(json!({
        "model_mappings": [{"from": "friendly-codex", "to": "gpt-5.6-sol"}]
    }));
    for path in ["/v1/messages", "/messages", "/v1/messages/"] {
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("application/json"));
        let built = build_upstream_request_with_bridge(
            &credential,
            "codex",
            path,
            Some("trace=messages"),
            headers,
            &serde_json::to_vec(&messages_request(true)).unwrap(),
            TurnReminderMode::Apply,
        )
        .expect("official Messages request");
        assert_eq!(
            built.target_url,
            "https://chatgpt.com/backend-api/codex/responses?trace=messages"
        );
        assert_eq!(
            built.bridge_kind,
            Some(ProtocolBridgeKind::ClaudeToResponses)
        );
        assert!(built.streaming_request);
        assert_eq!(built.headers["authorization"], "Bearer at-official-test");
        assert_eq!(built.headers["chatgpt-account-id"], "acct-official-test");
        assert_eq!(built.headers["accept"], "text/event-stream");
        let body: Value = serde_json::from_slice(&built.body).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["instructions"], "Use tools when needed.");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][1]["call_id"], "call-previous");
        assert_eq!(body["input"][1]["name"], "lookup");
        assert_eq!(body["input"][1]["arguments"], "{\"q\":\"previous\"}");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["call_id"], "call-previous");
        assert_eq!(body["input"][2]["output"], "previous-result");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["q"]["type"],
            "string"
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["store"], false);
        assert!(body.get("messages").is_none());
        assert!(body.get("system").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }
}

#[test]
fn official_codex_messages_relay_preserves_public_responses_limits() {
    let credential = official_credential(json!({"base_url": "https://relay.example/v1"}));
    let built = build_upstream_request_with_bridge(
        &credential,
        "codex",
        "/v1/messages",
        None,
        HeaderMap::new(),
        &serde_json::to_vec(&messages_request(false)).unwrap(),
        TurnReminderMode::Apply,
    )
    .unwrap();
    assert_eq!(built.target_url, "https://relay.example/v1/responses");
    assert_eq!(
        built.bridge_kind,
        Some(ProtocolBridgeKind::ClaudeToResponses)
    );
    assert!(!built.streaming_request);
    assert!(built.headers.get("chatgpt-account-id").is_none());
    let body: Value = serde_json::from_slice(&built.body).unwrap();
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(body["stream"], false);
}

async fn exercise_official_codex_messages_proxy(streaming: bool) {
    let (content_type, bytes) = proxy_official_request(
        "/v1/messages",
        messages_request(streaming),
        "ClaudeToResponses",
    )
    .await;
    if streaming {
        assert!(content_type.starts_with("text/event-stream"));
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("event: message_stop"));
        assert!(!text.contains("event: response."));
        assert!(!text.contains("data: [DONE]"));
        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data: ")
                    .and_then(|data| serde_json::from_str(data).ok())
            })
            .collect();
        let start = events
            .iter()
            .find(|event| event["type"] == "message_start")
            .unwrap();
        assert_eq!(start["message"]["type"], "message");
        assert_eq!(start["message"]["role"], "assistant");
        assert_eq!(start["message"]["usage"]["input_tokens"], 3);
        assert!(events.iter().any(
            |event| event["delta"]["type"] == "text_delta" && event["delta"]["text"] == "hello"
        ));
        let tool = events
            .iter()
            .find(|event| event["content_block"]["type"] == "tool_use")
            .unwrap();
        assert_eq!(tool["content_block"]["id"], "call-1");
        assert_eq!(tool["content_block"]["name"], "lookup");
        let arguments: String = events
            .iter()
            .filter(|event| event["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap(),
            json!({"q": "test"})
        );
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta["usage"]["output_tokens"], 2);
    } else {
        assert!(content_type.starts_with("application/json"));
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"][0], json!({"type": "text", "text": "hello"}));
        assert_eq!(
            body["content"][1],
            json!({
                "type": "tool_use", "id": "call-1", "name": "lookup", "input": {"q": "test"}
            })
        );
        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["usage"]["input_tokens"], 3);
        assert_eq!(body["usage"]["output_tokens"], 2);
        assert!(body.get("choices").is_none());
        assert!(body.get("output").is_none());
    }
}

#[tokio::test]
async fn official_codex_messages_proxy_returns_messages_json() {
    exercise_official_codex_messages_proxy(false).await;
}

#[tokio::test]
async fn official_codex_messages_proxy_returns_messages_sse() {
    exercise_official_codex_messages_proxy(true).await;
}

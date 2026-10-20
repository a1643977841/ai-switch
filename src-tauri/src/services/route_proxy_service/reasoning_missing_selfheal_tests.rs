use super::*;
use crate::database::repositories::route_credential_repository::RouteCredentialRepository;

/// End-to-end coverage for the `reasoning_content` missing (new-api code 11155)
/// self-heal. The proxy must, on a Chat-body gateway rejection, rewrite the
/// outbound body to carry `reasoning_content` on every assistant turn and
/// re-ask the *same* account once — and persist the account-level
/// `force_reasoning_content` flag so later turns never hit 11155 again.
///
/// Three wires are exercised:
///   * `ClaudeToChat` (Anthropic inbound, Chat upstream) — buffered path.
///   * A direct `Chat/Completions` passthrough (`None`) — streaming-prime path.
///   * `ResponsesToChat` — streaming-prime path (the original wired bridge).

type ReqLog = Arc<std::sync::Mutex<Vec<Value>>>;

struct HealFixture {
    requests: ReqLog,
    _server: tokio::task::JoinHandle<()>,
    pool: SqlitePool,
    credential_id: String,
}

impl Drop for HealFixture {
    fn drop(&mut self) {
        self._server.abort();
    }
}

/// Spins up a mock upstream that records every inbound request (decoded JSON) and
/// replays `responses` in call order, falling back to the last entry. Call 0 is
/// always the 11155 rejection; later calls succeed.
async fn start_heal_upstream(
    responses: Vec<(StatusCode, HeaderMap, String)>,
) -> (String, HealFixture) {
    let requests: ReqLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let responses = Arc::new(responses);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(move |body: axum::body::Bytes| {
                let recorded = recorded.clone();
                let responses = responses.clone();
                async move {
                    let index = {
                        let mut guard = recorded.lock().unwrap();
                        let index = guard.len();
                        let parsed =
                            serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                        guard.push(parsed);
                        index
                    };
                    let (status, headers, body_text) = responses
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| responses.last().cloned().unwrap());
                    let mut builder = Response::builder().status(status);
                    for (k, v) in headers.iter() {
                        builder = builder.header(k, v);
                    }
                    builder.body(Body::from(body_text)).unwrap()
                }
            }),
        )
        .await
        .unwrap();
    });
    // The pool/account are filled in by the caller; only the request log + server
    // live here until the caller attaches them into the fixture.
    let fixture = HealFixture {
        requests,
        _server: server,
        pool: create_memory_pool().await.unwrap(),
        credential_id: String::new(),
    };
    (base, fixture)
}

async fn wait_for_force_flag(pool: &SqlitePool, id: &str) -> bool {
    for _ in 0..100 {
        let cred = RouteCredentialRepository::get(pool, id).await.unwrap();
        let enabled = serde_json::from_str::<Value>(&cred.config_json)
            .ok()
            .and_then(|v| v.get("force_reasoning_content").cloned())
            .and_then(|v| v.as_bool())
            == Some(true);
        if enabled {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn sse(content_type: &str, body: &str) -> (StatusCode, HeaderMap, String) {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_str(content_type).unwrap(),
    );
    (StatusCode::OK, headers, body.to_string())
}

/// Anthropic → Chat bridge, non-streaming. The gateway rejects the first turn
/// with 11155; the proxy must heal by forcing `reasoning_content` onto the
/// assistant turn and re-ask the same account.
#[tokio::test]
async fn claude_to_chat_missing_reasoning_buffered_self_heal() {
    let (base, mut fixture) = start_heal_upstream(vec![
        (
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            json!({"code":11155,"message":"The `reasoning_content` in the thinking mode must be passed back to the API"})
                .to_string(),
        ),
        (
            StatusCode::OK,
            HeaderMap::new(),
            json!({"id":"resp","choices":[{"message":{"role":"assistant","content":"healed"}}],"model":"claude-sonnet-4"})
                .to_string(),
        ),
    ])
    .await;

    run_migrations(&fixture.pool).await.unwrap();
    let config = json!({
        "base_url": base,
        "interface_format": "openai",
        "model_mappings": [{"from":"claude-sonnet-4","to":"claude-sonnet-4"}],
        "failure_policy": {"retry_count": 1}
    });
    let credential = RouteCredentialRepository::create(
        &fixture.pool,
        "claude",
        "api",
        "claude-chat",
        None,
        "ok",
        None,
        r#"{"api_key":"sk-upstream"}"#,
        &config.to_string(),
        "{}",
    )
    .await
    .unwrap();
    fixture.credential_id = credential.id.clone();
    RoutePoolRepository::replace_members(&fixture.pool, "claude", &[credential.id.clone()])
        .await
        .unwrap();
    let route_key = RouteProxyKeyRepository::ensure_platform_key(
        &fixture.pool,
        "claude",
        "sk-test-claude-heal",
    )
    .await
    .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(fixture.pool.clone(), &runtime);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
    );
    let body = json!({
        "model":"claude-sonnet-4",
        "max_tokens":256,
        "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"prior answer"}
        ]
    })
    .to_string();
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        "/v1/messages".parse().unwrap(),
        Body::from(body),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "self-heal should recover and answer 200"
    );

    let recorded = fixture.requests.lock().unwrap();
    assert_eq!(
        recorded.len(),
        2,
        "expected one rejected turn then one healed retry: {recorded:?}"
    );
    let first = recorded.first().expect("first upstream request");
    assert!(
        !first.to_string().contains("reasoning_content"),
        "pre-heal request must not carry reasoning_content: {first}"
    );
    let second = recorded.get(1).expect("second upstream request");
    assert!(
        second.to_string().contains("reasoning_content")
            && second.to_string().contains("tool call"),
        "healed request must carry reasoning_content on the assistant turn: {second}"
    );
    drop(recorded);

    assert!(
        wait_for_force_flag(&fixture.pool, &fixture.credential_id).await,
        "account self-heal flag must be persisted"
    );
}

/// Direct Chat/Completions passthrough (`None` bridge), streaming. The upstream
/// opens a 200 SSE stream with a 11155 error frame; the proxy must catch it
/// during priming, heal, and retry the same account with `reasoning_content`.
#[tokio::test]
async fn chat_passthrough_prime_missing_reasoning_self_heal() {
    let err_frame = "data: {\"code\":11155,\"message\":\"The `reasoning_content` in the thinking mode must be passed back to the API\"}\n\n";
    let ok_stream = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"healed\"}}]}\n\ndata: [DONE]\n\n";
    let (base, mut fixture) = start_heal_upstream(vec![
        sse("text/event-stream", err_frame),
        sse("text/event-stream", ok_stream),
    ])
    .await;

    run_migrations(&fixture.pool).await.unwrap();
    let config = json!({
        "base_url": base,
        "interface_format": "openai",
        "model_mappings": [{"from":"gpt-5","to":"gpt-5"}],
        "failure_policy": {"retry_count": 1}
    });
    let credential = RouteCredentialRepository::create(
        &fixture.pool,
        "claude",
        "api",
        "chat-pass",
        None,
        "ok",
        None,
        r#"{"api_key":"sk-upstream"}"#,
        &config.to_string(),
        "{}",
    )
    .await
    .unwrap();
    fixture.credential_id = credential.id.clone();
    RoutePoolRepository::replace_members(&fixture.pool, "claude", &[credential.id.clone()])
        .await
        .unwrap();
    let route_key = RouteProxyKeyRepository::ensure_platform_key(
        &fixture.pool,
        "claude",
        "sk-test-chatpass-heal",
    )
    .await
    .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(fixture.pool.clone(), &runtime);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
    );
    let body = json!({
        "model":"gpt-5",
        "stream":true,
        "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"prior answer"}
        ]
    })
    .to_string();
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        "/v1/chat/completions".parse().unwrap(),
        Body::from(body),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "streaming self-heal should recover and answer 200"
    );

    let recorded = fixture.requests.lock().unwrap();
    assert_eq!(
        recorded.len(),
        2,
        "expected one prime failure then one healed retry: {recorded:?}"
    );
    let first = recorded.first().expect("first upstream request");
    assert!(
        !first.to_string().contains("reasoning_content"),
        "pre-heal stream request must not carry reasoning_content: {first}"
    );
    let second = recorded.get(1).expect("second upstream request");
    assert!(
        second.to_string().contains("reasoning_content")
            && second.to_string().contains("tool call"),
        "healed stream request must carry reasoning_content on the assistant turn: {second}"
    );
    drop(recorded);

    assert!(
        wait_for_force_flag(&fixture.pool, &fixture.credential_id).await,
        "account self-heal flag must be persisted"
    );
}

/// Responses → Chat bridge, streaming. This is the original wired bridge; assert
/// the prime-path self-heal still forces `reasoning_content` and persists the flag.
#[tokio::test]
async fn responses_to_chat_prime_missing_reasoning_self_heal() {
    let err_frame = "data: {\"code\":11155,\"message\":\"The `reasoning_content` in the thinking mode must be passed back to the API\"}\n\n";
    let ok_stream = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"healed\"}}]}\n\ndata: [DONE]\n\n";
    let (base, mut fixture) = start_heal_upstream(vec![
        sse("text/event-stream", err_frame),
        sse("text/event-stream", ok_stream),
    ])
    .await;

    run_migrations(&fixture.pool).await.unwrap();
    let config = json!({
        "base_url": base,
        "interface_format": "openai",
        "model_mappings": [{"from":"gpt-5","to":"gpt-5"}],
        "failure_policy": {"retry_count": 1}
    });
    let credential = RouteCredentialRepository::create(
        &fixture.pool,
        "codex",
        "api",
        "resp-chat",
        None,
        "ok",
        None,
        r#"{"api_key":"sk-upstream"}"#,
        &config.to_string(),
        "{}",
    )
    .await
    .unwrap();
    fixture.credential_id = credential.id.clone();
    RoutePoolRepository::replace_members(&fixture.pool, "codex", &[credential.id.clone()])
        .await
        .unwrap();
    let route_key = RouteProxyKeyRepository::ensure_platform_key(
        &fixture.pool,
        "codex",
        "sk-test-respchat-heal",
    )
    .await
    .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(fixture.pool.clone(), &runtime);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
    );
    let body = json!({
        "model":"gpt-5",
        "stream":true,
        "input":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"prior answer"}
        ],
        "tools":[]
    })
    .to_string();
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        "/v1/responses".parse().unwrap(),
        Body::from(body),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "streaming self-heal should recover and answer 200"
    );

    let recorded = fixture.requests.lock().unwrap();
    assert_eq!(
        recorded.len(),
        2,
        "expected one prime failure then one healed retry: {recorded:?}"
    );
    let first = recorded.first().expect("first upstream request");
    assert!(
        !first.to_string().contains("reasoning_content"),
        "pre-heal stream request must not carry reasoning_content: {first}"
    );
    let second = recorded.get(1).expect("second upstream request");
    assert!(
        second.to_string().contains("reasoning_content")
            && second.to_string().contains("tool call"),
        "healed stream request must carry reasoning_content on the assistant turn: {second}"
    );
    drop(recorded);

    assert!(
        wait_for_force_flag(&fixture.pool, &fixture.credential_id).await,
        "account self-heal flag must be persisted"
    );
}

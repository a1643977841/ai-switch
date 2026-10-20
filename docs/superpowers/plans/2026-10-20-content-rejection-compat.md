# 内容过滤拒绝兼容 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 把上游内容过滤拒绝从账号健康故障中分流，并在账号级显式开启时为同一失败重试一次“省略较旧工具输出”的请求。

**Architecture:** 先让 `sensitive_words_detected` / `content-blocked` 进入独立的 A 分流：不消耗重试预算、不写健康失败、不冷却，只记录提示；聚合模式仍由现有候选队列自然换号。第一轮候选全部结束后，若至少一个被拒绝账号开启了 B1，则只重写工具输出一次，并只重试这些账号。换号逻辑本身不新增旁路，精确模式仍只有一个候选。

**Tech Stack:** Rust（axum / reqwest / sqlx / tokio / serde_json）、Tauri v2、React + TypeScript + Vitest。

**Spec:** `docs/superpowers/specs/2026-10-20-content-rejection-compat-design.md`

## Global Constraints

- 直接在 `main` 工作，不创建分支或 worktree。
- Rust 的 AI 调试、测试和验证统一使用 `src-tauri/target-codex/`；在 `src-tauri` 下执行命令时必须设置 `CARGO_TARGET_DIR=target-codex`。
- 不创建 `target-codex-test`、`target-codex-pairing`、根目录 `target/` 或任何第三个 Cargo target。
- A 只识别结构化 `error.code` 的完整值：`sensitive_words_detected`、`content-blocked`；不得按消息文本扩大匹配。
- A 不修改 `transient_failure_count`、`next_retry_at`、`cooldown_until`、`semantic_failure_streak_*` 或账号 `status`。
- 换号只来自聚合模式现有队列；精确模式与 `failover_enabled = false` 均不得新增跨账号尝试。
- B1 是账号级开关，配置键为 `content_rejection_compat`，唯一开启值为 `trim_old_tool_outputs`，缺失或 `off` 为关闭。
- B1 每个客户端请求最多重写一次、最多进入第二轮一次。
- B1 只修改 Responses 的 `function_call_output` / `custom_tool_call_output` / `local_shell_call_output` 与 Chat 的 `role = "tool"` 内容。
- B1 保留最新 6 条工具输出；只替换更早的、正文超过 128 字节的纯文本输出。
- 不修改 user/assistant/system/developer、`instructions`、`tools`、工具参数、`call_id` / `tool_call_id`、reasoning 或加密内容。
- 不使用 Brotli/Gzip/Zstd、零宽字符、同形字或编码混淆绕过内容过滤。

---

### Task 1: 识别内容拒绝并固定故障作用域

**Files:**
- Modify: `src-tauri/src/services/response_failure_service.rs`
- Modify: `src-tauri/src/services/route_failure_scope.rs`

**Interfaces:**
- Consumes: `SemanticResponseFailure { code: Option<String>, error_type: Option<String>, message: String }` 与现有 `detect_response_failed`。
- Produces: `pub(crate) fn is_content_rejection(failure: &SemanticResponseFailure) -> bool`。
- Produces: `is_account_scoped_failure("content_rejected", status)` 返回 `false`。

- [x] **Step 1: 在 `response_failure_service.rs` 的测试模块写失败测试**

在现有 `detect_response_failed_value` / 配额测试附近加入：

```rust
#[test]
fn recognizes_content_rejection_codes_as_complete_case_insensitive_values() {
    for code in ["sensitive_words_detected", "CONTENT-BLOCKED"] {
        let failure = SemanticResponseFailure {
            code: Some(code.to_string()),
            error_type: Some("new_api_error".to_string()),
            message: "opaque upstream text".to_string(),
        };
        assert!(is_content_rejection(&failure), "{code}");
    }

    for failure in [
        SemanticResponseFailure {
            code: Some("sensitive_words_detected_extra".to_string()),
            error_type: None,
            message: "sensitive_words_detected".to_string(),
        },
        SemanticResponseFailure {
            code: Some("upstream_status".to_string()),
            error_type: None,
            message: "content-blocked".to_string(),
        },
        SemanticResponseFailure {
            code: None,
            error_type: None,
            message: "sensitive words detected".to_string(),
        },
    ] {
        assert!(!is_content_rejection(&failure));
    }
}
```

在 `route_failure_scope.rs` 的测试模块加入：

```rust
#[test]
fn content_rejections_do_not_park_the_account() {
    assert!(!is_account_scoped_failure("content_rejected", Some(500)));
    assert!(!is_account_scoped_failure("content_rejected", None));
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection
```

Expected: 编译失败，提示 `is_content_rejection` 未定义；现有 `content_rejections_do_not_park_the_account` 若先单独运行则断言失败。

- [x] **Step 3: 写最小实现**

在 `response_failure_service.rs` 的 `SemanticResponseFailure` 定义之后加入：

```rust
const CONTENT_REJECTION_CODES: &[&str] = &[
    "sensitive_words_detected",
    "content-blocked",
];

/// Whether the upstream explicitly rejected the request body by its content
/// policy. Match the complete structured code only: the same phrase can appear
/// in ordinary prose, while a gateway's `upstream_status` code is not evidence
/// that content was filtered.
pub(crate) fn is_content_rejection(failure: &SemanticResponseFailure) -> bool {
    failure.code.as_deref().is_some_and(|code| {
        let code = code.trim();
        CONTENT_REJECTION_CODES
            .iter()
            .any(|known| code.eq_ignore_ascii_case(known))
    })
}
```

在 `route_failure_scope.rs` 的 `semantic_response_transient` 分支旁加入：

```rust
// 内容级拒绝不是账号或网络故障。即使当前调用路径已经绕过健康记账，
// 显式分类也能防止未来的 `_ => true` 兜底把它误当成账号级失败。
"content_rejected" => false,
```

- [x] **Step 4: 运行测试并确认通过**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib route_failure_scope
```

Expected: 两个过滤条件均通过。

- [x] **Step 5: 格式化并提交**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/services/response_failure_service.rs src-tauri/src/services/route_failure_scope.rs
git commit -m "feat(路由): 识别内容过滤拒绝"
```

---

### Task 2: 记录内容拒绝提示但不记账号健康

**Files:**
- Modify: `src-tauri/src/database/repositories/route_credential_repository.rs`

**Interfaces:**
- Consumes: `truncate_failure_message(&str) -> String`、`truncate_failure_response(Option<&[u8]>) -> Option<String>`、`database_error`。
- Produces: `RouteCredentialRepository::record_content_rejection(pool: &SqlitePool, id: &str, message: &str, response_body: Option<&[u8]>) -> Result<(), AppError>`。
- Produces: 当账号存在生效中的 `cooldown_until` / `next_retry_at` 时，该方法返回 `Ok(())` 且不覆盖已有 `last_failure_*`。

- [x] **Step 1: 写仓库失败测试**

在 `route_credential_repository.rs` 测试模块最后加入：

```rust
#[tokio::test]
async fn content_rejection_records_only_last_failure_fields() {
    let pool = crate::database::create_memory_pool().await.unwrap();
    crate::database::run_migrations(&pool).await.unwrap();
    let created = create_api_credential(&pool, "codex", "Content hint").await;

    sqlx::query(
        "UPDATE route_credentials
         SET transient_failure_count = 4,
             semantic_failure_streak_count = 7,
             semantic_failure_streak_fingerprint = 'keep-me'
         WHERE id = ?",
    )
    .bind(&created.id)
    .execute(&pool)
    .await
    .unwrap();

    RouteCredentialRepository::record_content_rejection(
        &pool,
        &created.id,
        "sensitive words detected (request id: test)",
        Some(br#"{"error":{"code":"sensitive_words_detected"}}"#),
    )
    .await
    .expect("record content rejection");

    let stored = RouteCredentialRepository::get(&pool, &created.id)
        .await
        .expect("row");
    assert_eq!(stored.status, "ok");
    assert_eq!(stored.transient_failure_count, 4);
    assert!(stored.next_retry_at.is_none());
    assert!(stored.cooldown_until.is_none());
    assert_eq!(stored.last_failure_kind.as_deref(), Some("content_rejected"));
    assert_eq!(
        stored.last_failure_message.as_deref(),
        Some("sensitive words detected (request id: test)")
    );
    assert_eq!(
        stored.last_failure_response_json.as_deref(),
        Some(r#"{"error":{"code":"sensitive_words_detected"}}"#)
    );

    let streak: (i64, Option<String>) = sqlx::query_as(
        "SELECT semantic_failure_streak_count, semantic_failure_streak_fingerprint
         FROM route_credentials WHERE id = ?",
    )
    .bind(&created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(streak, (7, Some("keep-me".to_string())));
}

#[tokio::test]
async fn content_rejection_does_not_overwrite_an_active_failure() {
    let pool = crate::database::create_memory_pool().await.unwrap();
    crate::database::run_migrations(&pool).await.unwrap();
    let created = create_api_credential(&pool, "codex", "Cooling").await;
    let future = "2099-01-01T00:00:00+00:00";
    sqlx::query(
        "UPDATE route_credentials
         SET transient_failure_count = 2, next_retry_at = ?, cooldown_until = ?,
             last_failure_kind = 'transport', last_failure_message = 'connection reset',
             last_failure_response_json = '{\"error\":\"keep\"}'
         WHERE id = ?",
    )
    .bind(future)
    .bind(future)
    .bind(&created.id)
    .execute(&pool)
    .await
    .unwrap();

    RouteCredentialRepository::record_content_rejection(
        &pool,
        &created.id,
        "sensitive words detected",
        Some(br#"{"error":{"code":"sensitive_words_detected"}}"#),
    )
    .await
    .expect("preserve active failure");

    let stored = RouteCredentialRepository::get(&pool, &created.id)
        .await
        .expect("row");
    assert_eq!(stored.cooldown_until.as_deref(), Some(future));
    assert_eq!(stored.last_failure_kind.as_deref(), Some("transport"));
    assert_eq!(stored.last_failure_message.as_deref(), Some("connection reset"));
}

#[tokio::test]
async fn content_rejection_on_a_missing_account_is_a_validation_error() {
    let pool = crate::database::create_memory_pool().await.unwrap();
    crate::database::run_migrations(&pool).await.unwrap();

    let error = RouteCredentialRepository::record_content_rejection(
        &pool,
        "missing",
        "sensitive words detected",
        None,
    )
    .await
    .expect_err("missing account");
    assert!(matches!(
        error,
        AppError::Validation {
            code: "validation.route_credential_not_found",
            ..
        }
    ));
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection_records_only
```

Expected: 编译失败，提示 `record_content_rejection` 未定义。

- [x] **Step 3: 写最小实现**

在 `RouteCredentialRepository` 的 `clear_transient_failure` 附近加入：

```rust
/// Record an explicit content-policy rejection for the account tooltip without
/// charging it as an account-health failure. A cooling account keeps the
/// failure that caused the cooldown: replacing it would hide the more
/// actionable problem while the older rejection is still in force.
pub async fn record_content_rejection(
    pool: &SqlitePool,
    id: &str,
    message: &str,
    response_body: Option<&[u8]>,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(|err| {
        database_error(
            "database.route_credential_content_rejection_tx",
            "Could not start content rejection update",
            err,
        )
    })?;
    let state = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT cooldown_until, next_retry_at FROM route_credentials WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|err| {
        database_error(
            "database.route_credential_content_rejection_read",
            "Could not read content rejection state",
            err,
        )
    })?;
    let Some((cooldown_until, next_retry_at)) = state else {
        return Err(AppError::Validation {
            code: "validation.route_credential_not_found",
            message: "Route credential does not exist".to_string(),
            details: Some(id.to_string()),
            recoverable: true,
        });
    };

    let now = Utc::now();
    let still_failing = [cooldown_until.as_deref(), next_retry_at.as_deref()]
        .into_iter()
        .flatten()
        .any(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .map(|deadline| deadline.with_timezone(&Utc) > now)
                // A malformed deadline is unknown state; preserve the existing
                // failure rather than overwrite something we cannot interpret.
                .unwrap_or(true)
        });
    if still_failing {
        tx.commit().await.map_err(|err| {
            database_error(
                "database.route_credential_content_rejection_commit",
                "Could not finish content rejection check",
                err,
            )
        })?;
        return Ok(());
    }

    let now = now.to_rfc3339();
    let result = sqlx::query(
        "UPDATE route_credentials
         SET last_failure_kind = 'content_rejected',
             last_failure_message = ?,
             last_failure_response_json = ?,
             updated_at = ?
         WHERE id = ?",
    )
    .bind(truncate_failure_message(message))
    .bind(truncate_failure_response(response_body))
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(|err| {
        database_error(
            "database.route_credential_content_rejection",
            "Could not record content rejection",
            err,
        )
    })?;
    if result.rows_affected() == 0 {
        return Err(AppError::Validation {
            code: "validation.route_credential_not_found",
            message: "Route credential does not exist".to_string(),
            details: Some(id.to_string()),
            recoverable: true,
        });
    }
    tx.commit().await.map_err(|err| {
        database_error(
            "database.route_credential_content_rejection_commit",
            "Could not save content rejection",
            err,
        )
    })?;
    Ok(())
}
```

- [x] **Step 4: 运行测试并确认通过**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection
```

Expected: 三个仓库测试与 Task 1 的识别测试全部通过。

- [x] **Step 5: 格式化并提交**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/database/repositories/route_credential_repository.rs
git commit -m "feat(路由): 记录内容过滤提示不记健康失败"
```

---

### Task 3: 实现旧工具输出裁剪模块

**Files:**
- Create: `src-tauri/src/services/request_content_rejection.rs`
- Modify: `src-tauri/src/services/mod.rs`

**Interfaces:**
- Consumes: `serde_json::Value`、`AppError`。
- Produces: `pub(crate) const CONFIG_KEY: &str = "content_rejection_compat";`
- Produces: `pub(crate) fn validate(mode: Option<&Value>) -> Result<(), AppError>;`
- Produces: `pub(crate) fn trim_enabled(config: &Value) -> bool;`
- Produces: `pub(crate) fn elide_old_tool_outputs(body: &[u8]) -> Option<Vec<u8>>;`
- Produces: `const ELIDED_TOOL_OUTPUT: &str` 与 `const KEEP_LATEST_TOOL_OUTPUTS: usize = 6`。

- [x] **Step 1: 写裁剪器失败测试**

新建文件并在实现之前只放测试与模块骨架；测试代码必须包含以下完整用例：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn long(label: &str) -> String {
        format!("{label}-{}", "x".repeat(160))
    }

    #[test]
    fn keeps_the_latest_six_responses_outputs_and_elides_the_oldest() {
        let input = (0..7)
            .map(|index| {
                json!({
                    "type": "function_call_output",
                    "call_id": format!("call-{index}"),
                    "output": long(&index.to_string())
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"model":"gpt-5","input":input})).unwrap();

        let trimmed = elide_old_tool_outputs(&body).expect("one old output trimmed");
        let value: Value = serde_json::from_slice(&trimmed).unwrap();
        assert_eq!(value["input"][0]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(value["input"][0]["call_id"], "call-0");
        for index in 1..7 {
            assert!(value["input"][index]["output"].as_str().unwrap().ends_with("xxxxxxxxxx") ||
                value["input"][index]["output"].as_str().unwrap().len() > 128);
        }
        assert_eq!(elide_old_tool_outputs(&trimmed), None);
    }

    #[test]
    fn six_outputs_need_no_rewrite() {
        let input = (0..6)
            .map(|index| {
                json!({
                    "type": "function_call_output",
                    "call_id": format!("call-{index}"),
                    "output": long(&index.to_string())
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"input":input})).unwrap();
        assert_eq!(elide_old_tool_outputs(&body), None);
    }

    #[test]
    fn only_tool_output_bodies_change() {
        let body = serde_json::to_vec(&json!({
            "model": "gpt-5",
            "instructions": long("instructions"),
            "tools": [{"type":"function","name":"Bash","parameters":{"type":"object"}}],
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":long("user")}]},
                {"type":"function_call","call_id":"call-0","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
                {"type":"function_call_output","call_id":"call-0","output":long("old")},
                {"type":"custom_tool_call_output","call_id":"call-1","output":long("old-custom")},
                {"type":"local_shell_call_output","call_id":"call-2","output":long("old-shell")},
                {"type":"function_call_output","call_id":"call-3","output":long("new-3")},
                {"type":"function_call_output","call_id":"call-4","output":long("new-4")},
                {"type":"function_call_output","call_id":"call-5","output":long("new-5")},
                {"type":"function_call_output","call_id":"call-6","output":long("new-6")},
                {"type":"function_call_output","call_id":"call-7","output":long("new-7")},
                {"type":"function_call_output","call_id":"call-8","output":long("new-8")}
            ]
        }))
        .unwrap();
        let before: Value = serde_json::from_slice(&body).unwrap();
        let after: Value = serde_json::from_slice(&elide_old_tool_outputs(&body).unwrap()).unwrap();

        assert_eq!(after["instructions"], before["instructions"]);
        assert_eq!(after["tools"], before["tools"]);
        assert_eq!(after["input"][0], before["input"][0]);
        assert_eq!(after["input"][1], before["input"][1]);
        assert_eq!(after["input"][2]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(after["input"][3]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(after["input"][4]["output"], ELIDED_TOOL_OUTPUT);
        for index in 5..11 {
            assert_eq!(after["input"][index], before["input"][index]);
        }
    }

    #[test]
    fn chat_tool_messages_use_a_single_text_part() {
        let messages = (0..7)
            .map(|index| {
                json!({
                    "role": "tool",
                    "tool_call_id": format!("call-{index}"),
                    "content": [{"type":"text","text":long(&index.to_string())}]
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"model":"gpt-5","messages":messages})).unwrap();
        let value: Value = serde_json::from_slice(&elide_old_tool_outputs(&body).unwrap()).unwrap();
        assert_eq!(
            value["messages"][0]["content"],
            json!([{"type":"text","text":ELIDED_TOOL_OUTPUT}])
        );
        assert_eq!(value["messages"][0]["tool_call_id"], "call-0");
    }

    #[test]
    fn non_text_parts_are_skipped_while_other_old_text_outputs_are_trimmed() {
        let with_image = serde_json::to_vec(&json!({
            "input": [
                {"type":"function_call_output","call_id":"0","output":[
                    {"type":"input_text","text":long("old")},
                    {"type":"input_image","image_url":"data:image/png;base64,AA=="}
                ]},
                {"type":"function_call_output","call_id":"1","output":long("1")},
                {"type":"function_call_output","call_id":"2","output":long("2")},
                {"type":"function_call_output","call_id":"3","output":long("3")},
                {"type":"function_call_output","call_id":"4","output":long("4")},
                {"type":"function_call_output","call_id":"5","output":"short"},
                {"type":"function_call_output","call_id":"6","output":long("6")}
            ]
        }))
        .unwrap();
        let value: Value =
            serde_json::from_slice(&elide_old_tool_outputs(&with_image).unwrap()).unwrap();
        assert_eq!(
            value["input"][0]["output"],
            json!([
                {"type":"input_text","text":long("old")},
                {"type":"input_image","image_url":"data:image/png;base64,AA=="}
            ])
        );
        assert_eq!(value["input"][1]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(value["input"][6]["output"], long("6"));
        assert_eq!(elide_old_tool_outputs(b"not json"), None);
        assert!(!trim_enabled(&json!({})));
        assert!(!trim_enabled(&json!({CONFIG_KEY:"off"})));
        assert!(trim_enabled(&json!({CONFIG_KEY:"trim_old_tool_outputs"})));
    }

    #[test]
    fn validation_accepts_only_the_supported_modes() {
        for mode in [
            None,
            Some(Value::Null),
            Some(json!("off")),
            Some(json!("trim_old_tool_outputs")),
        ] {
            assert!(validate(mode.as_ref()).is_ok());
        }
        for mode in [json!("on"), json!(true), json!("trim_all")] {
            assert!(validate(Some(&mode)).is_err());
        }
    }
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib request_content_rejection
```

Expected: 编译失败，提示模块常量与函数未定义。

- [x] **Step 3: 写最小实现**

文件顶部加入：

```rust
//! Per-account request rewrite used only after an upstream explicitly rejects
//! the request by its content policy.

use crate::error::AppError;
use serde_json::{json, Map, Value};

pub(crate) const CONFIG_KEY: &str = "content_rejection_compat";
pub(crate) const ELIDED_TOOL_OUTPUT: &str =
    "[ai-switch: older tool output omitted after upstream content filter; re-run the command if needed]";
const KEEP_LATEST_TOOL_OUTPUTS: usize = 6;
const MIN_ELIDED_BYTES: usize = 128;

pub(crate) fn validate(mode: Option<&Value>) -> Result<(), AppError> {
    if matches!(mode, None | Some(Value::Null))
        || mode.and_then(Value::as_str).is_some_and(|mode| {
            matches!(mode, "off" | "trim_old_tool_outputs")
        })
    {
        return Ok(());
    }
    Err(AppError::Validation {
        code: "validation.content_rejection_compat",
        message: "内容过滤兼容仅支持 off 或 trim_old_tool_outputs。".to_string(),
        details: None,
        recoverable: true,
    })
}

pub(crate) fn trim_enabled(config: &Value) -> bool {
    config.get(CONFIG_KEY).and_then(Value::as_str)
        == Some("trim_old_tool_outputs")
}
```

实现的两个入口与辅助函数：

```rust
#[derive(Debug, Clone, Copy)]
enum ToolOutputSlot {
    Responses(usize),
    Chat(usize),
}

pub(crate) fn elide_old_tool_outputs(body: &[u8]) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let object = value.as_object_mut()?;
    let slots = collect_tool_output_slots(object);
    let elide_until = slots.len().saturating_sub(KEEP_LATEST_TOOL_OUTPUTS);
    if elide_until == 0 {
        return None;
    }

    let mut changed = false;
    for slot in slots.into_iter().take(elide_until) {
        changed |= match slot {
            ToolOutputSlot::Responses(index) => object
                .get_mut("input")
                .and_then(Value::as_array_mut)
                .and_then(|items| items.get_mut(index))
                .is_some_and(|item| elide_responses_output(item)),
            ToolOutputSlot::Chat(index) => object
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .and_then(|messages| messages.get_mut(index))
                .is_some_and(|message| elide_chat_output(message)),
        };
    }
    changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

fn collect_tool_output_slots(object: &Map<String, Value>) -> Vec<ToolOutputSlot> {
    let mut slots = Vec::new();
    if let Some(items) = object.get("input").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "function_call_output"
                            | "custom_tool_call_output"
                            | "local_shell_call_output"
                    )
                })
            {
                slots.push(ToolOutputSlot::Responses(index));
            }
        }
    }
    if let Some(messages) = object.get("messages").and_then(Value::as_array) {
        for (index, message) in messages.iter().enumerate() {
            if message.get("role").and_then(Value::as_str) == Some("tool") {
                slots.push(ToolOutputSlot::Chat(index));
            }
        }
    }
    slots
}

fn elide_responses_output(item: &mut Value) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    let Some(output) = object.get_mut("output") else {
        return false;
    };
    elide_value(output, true)
}

fn elide_chat_output(message: &mut Value) -> bool {
    let Some(content) = message.as_object_mut().and_then(|object| object.get_mut("content")) else {
        return false;
    };
    elide_value(content, false)
}

fn elide_value(output: &mut Value, responses_shape: bool) -> bool {
    match output {
        Value::String(text) if text.len() > MIN_ELIDED_BYTES => {
            *text = ELIDED_TOOL_OUTPUT.to_string();
            true
        }
        Value::Array(parts) => {
            let mut total = 0usize;
            let mut all_text = !parts.is_empty();
            for part in parts.iter() {
                let Some(object) = part.as_object() else {
                    all_text = false;
                    break;
                };
                let kind = object.get("type").and_then(Value::as_str);
                let text = object.get("text").and_then(Value::as_str);
                let accepted_kind = kind.is_some_and(|kind| {
                    matches!(kind, "text" | "input_text" | "output_text")
                });
                let Some(text) = text.filter(|_| accepted_kind) else {
                    all_text = false;
                    break;
                };
                total = total.saturating_add(text.len());
            }
            if !all_text || total <= MIN_ELIDED_BYTES {
                return false;
            }
            *parts = vec![if responses_shape {
                json!({"type":"input_text","text":ELIDED_TOOL_OUTPUT})
            } else {
                json!({"type":"text","text":ELIDED_TOOL_OUTPUT})
            }];
            true
        }
        _ => false,
    }
}
```

在 `src-tauri/src/services/mod.rs` 的 `request_compression` 旁加入：

```rust
pub(crate) mod request_content_rejection;
```

- [x] **Step 4: 运行测试并确认通过**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib request_content_rejection
```

Expected: 全部裁剪器测试通过；图片数组、短输出、非 JSON 返回 `None`。

- [x] **Step 5: 格式化并提交**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/services/request_content_rejection.rs src-tauri/src/services/mod.rs
git commit -m "feat(路由): 添加旧工具输出裁剪器"
```

---

### Task 4: 在代理主路径接入 A 分流

**Files:**
- Modify: `src-tauri/src/services/route_proxy_service.rs`
- Test: `src-tauri/src/services/route_proxy_service.rs`（现有内嵌 `tests` 模块）

**Interfaces:**
- Consumes: `is_content_rejection`、`RouteCredentialRepository::record_content_rejection`。
- Produces: 主循环局部变量 `rejected_indexes: Vec<usize>`，记录本轮被内容过滤拒绝的候选下标。
- Produces: `handle_stream_prime_failure` 新增 `rejected_indexes: &mut Vec<usize>` 参数；内容拒绝返回 `false` 且不重新入队。

- [x] **Step 1: 写 buffered 与 SSE 首帧失败测试**

在 `route_proxy_service.rs` 的测试模块加入：

```rust
#[tokio::test]
async fn content_rejection_fails_over_without_retrying_or_penalizing_the_account() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"sensitive_words_detected","message":"sensitive words detected (request id: test)","type":"new_api_error"}}"#;
    let (first, first_calls) = start_status_sequence_upstream(
        usize::MAX,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":true}"#,
    )
    .await;
    let (second, second_calls) = start_status_sequence_upstream(
        0,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":true}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let first_id = create_proxy_api_credential_with_config(
        &pool,
        "content-a",
        &first,
        json!({"failure_policy":{"retry_count":2,"retry_interval_ms":0}}),
    )
    .await;
    let second_id = create_proxy_api_credential_with_config(
        &pool,
        "content-b",
        &second,
        json!({"failure_policy":{"retry_count":2,"retry_interval_ms":0}}),
    )
    .await;
    sqlx::query("UPDATE route_credentials SET route_priority = 1 WHERE id = ?")
        .bind(&first_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE route_credentials SET route_priority = 2 WHERE id = ?")
        .bind(&second_id)
        .execute(&pool)
        .await
        .unwrap();
    RoutePoolRepository::replace_members(&pool, "codex", &[first_id.clone(), second_id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-content")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
    );

    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        "/v1/chat/completions".parse().unwrap(),
        Body::from(
            json!({"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}).to_string(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
        r#"{"ok":true}"#
    );
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    let first = RouteCredentialRepository::get(&pool, &first_id)
        .await
        .expect("first account");
    assert_eq!(first.status, "ok");
    assert_eq!(first.transient_failure_count, 0);
    assert!(first.cooldown_until.is_none());
    assert_eq!(first.last_failure_kind.as_deref(), Some("content_rejected"));
}

#[tokio::test]
async fn stream_prime_content_rejection_does_not_retry_the_same_account() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE_FRAME: &str = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"sensitive_words_detected\",\"message\":\"sensitive words detected\",\"type\":\"new_api_error\"}}}\n\n";
    let (upstream, calls) = start_scripted_sse_upstream(vec![vec![SENSITIVE_FRAME]]).await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let id = create_proxy_api_credential_with_config(
        &pool,
        "stream-content",
        &upstream,
        json!({"interface_format":"openai-responses","failure_policy":{"retry_count":2,"retry_interval_ms":0}}),
    )
    .await;
    RoutePoolRepository::replace_members(&pool, "codex", std::slice::from_ref(&id))
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-stream")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
    );

    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        headers,
        "/v1/responses".parse().unwrap(),
        Body::from(
            json!({"model":"gpt-5","stream":true,"input":"hi"}).to_string(),
        ),
    )
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        String::from_utf8_lossy(&body).contains("sensitive words detected"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "A must not retry the same account"
    );
    let account = RouteCredentialRepository::get(&pool, &id)
        .await
        .expect("account");
    assert_eq!(account.status, "ok");
    assert_eq!(account.transient_failure_count, 0);
    assert_eq!(account.last_failure_kind.as_deref(), Some("content_rejected"));
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection_fails_over
```

Expected: 当前实现把它当普通 500，第一账号被同号重试三次；`first_calls` 为 3 而不是 1。

- [x] **Step 3: 写最小实现**

把 `is_content_rejection` 加入 `route_proxy_service.rs` 的 `response_failure_service` import。

在主循环 `let mut retry_errors = Vec::new();` 旁加入：

```rust
let mut rejected_indexes = Vec::new();
```

在 `if quota_failure || quota_exhausted` **之前**、现有 reasoning / encrypted-content 特判之后加入：

```rust
// A content-policy rejection is deterministic for this body. Consume this
// candidate without touching its retry budget or health, then let the existing
// queue decide whether aggregate mode has another account to try.
if let Some(failure) = semantic_failure
    .as_ref()
    .filter(|failure| is_content_rejection(failure))
{
    if RouteCredentialRepository::record_content_rejection(
        pool,
        &credential.id,
        &failure.message,
        Some(&response_bytes),
    )
    .await
    .is_ok()
    {
        state
            .activity
            .notify_status_change(&platform, &credential.id);
    }
    rejected_indexes.push(credential_index);
    retry_errors.push(format!("{}: {}", credential.display_name, failure.message));
    stop_failover_when_disabled(&mut retry_queue, failure_policy.failover_enabled);
    continue;
}
```

在流式分支调用 `handle_stream_prime_failure` 时传入 `&mut rejected_indexes`。在 helper 参数中加入：

```rust
rejected_indexes: &mut Vec<usize>,
```

在 helper 中 `context_length` 特判之后、quota 特判之前加入：

```rust
if let Some(failure) = context
    .response_body
    .and_then(detect_response_failed)
    .filter(is_content_rejection)
{
    if RouteCredentialRepository::record_content_rejection(
        pool,
        &credential.id,
        &failure.message,
        context.response_body,
    )
    .await
    .is_ok()
    {
        state
            .activity
            .notify_status_change(platform, &credential.id);
    }
    rejected_indexes.push(credential_index);
    retry_errors.push(format!("{}: {}", credential.display_name, failure.message));
    stop_failover_when_disabled(retry_queue, failure_policy.failover_enabled);
    return false;
}
```

该分支必须在 `retry_same_credential` 分支之前，确保 SSE 200 首帧拒绝也不会被记成 `"transport"` 或同号重试。

- [x] **Step 4: 运行测试并确认通过**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection
```

Expected: buffered、SSE、作用域、仓库和裁剪器测试全部通过；两个新代理测试都只增加 A 分流，不触发 B1。

- [x] **Step 5: 格式化并提交**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/services/route_proxy_service.rs
git commit -m "feat(路由): 内容过滤拒绝不重试同账号"
```

---

### Task 5: 接入 B1 两轮重试并覆盖模式边界

**Files:**
- Modify: `src-tauri/src/services/route_proxy_service.rs`
- Test: `src-tauri/src/services/route_proxy_service.rs`（现有内嵌 `tests` 模块）

**Interfaces:**
- Consumes: `request_content_rejection::trim_enabled(&Value) -> bool`。
- Consumes: `request_content_rejection::elide_old_tool_outputs(&[u8]) -> Option<Vec<u8>>`。
- Consumes: Task 4 产出的 `rejected_indexes: Vec<usize>`。
- Produces: `fn content_rejection_retry_indexes(credentials: &[SelectedCredential], rejected_indexes: &[usize]) -> Vec<usize>`。
- Produces: 主转发循环外层 `'attempts: loop`；第二轮仅由 `content_rejection_retry_indexes` 构造，且 `trimmed_content_rejection_retry = true` 后不会产生第三轮。

- [x] **Step 1: 写 B1 失败测试**

在测试模块加入以下完整用例：

```rust
fn b1_chat_body(model: &str) -> Value {
    json!({
        "model": model,
        "messages": (0..7).map(|index| {
            json!({
                "role": "tool",
                "tool_call_id": format!("call-{index}"),
                "content": b1_old_tool_output(index)
            })
        }).collect::<Vec<_>>()
    })
}

fn b1_old_tool_output(index: usize) -> String {
    format!("tool-output-{index}-{}", "x".repeat(160))
}

#[tokio::test]
async fn content_rejection_compat_retries_only_the_enabled_rejected_account() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"sensitive_words_detected","message":"sensitive words detected"}}"#;
    let (enabled, enabled_calls) = start_status_sequence_upstream(
        1,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":"trimmed"}"#,
    )
    .await;
    let (disabled, disabled_calls) = start_status_sequence_upstream(
        usize::MAX,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":"never"}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let enabled_id = create_proxy_api_credential_with_config(
        &pool,
        "enabled",
        &enabled,
        json!({
            "content_rejection_compat":"trim_old_tool_outputs",
            "failure_policy":{"retry_count":0,"retry_interval_ms":0}
        }),
    )
    .await;
    let disabled_id = create_proxy_api_credential_with_config(
        &pool,
        "disabled",
        &disabled,
        json!({"failure_policy":{"retry_count":0,"retry_interval_ms":0}}),
    )
    .await;
    sqlx::query("UPDATE route_credentials SET route_priority = 1 WHERE id = ?")
        .bind(&enabled_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE route_credentials SET route_priority = 2 WHERE id = ?")
        .bind(&disabled_id)
        .execute(&pool)
        .await
        .unwrap();
    RoutePoolRepository::replace_members(&pool, "codex", &[enabled_id.clone(), disabled_id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-b1")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
            );
            headers
        },
        "/v1/chat/completions".parse().unwrap(),
        Body::from(b1_chat_body("gpt-5").to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
        r#"{"ok":"trimmed"}"#
    );
    assert_eq!(enabled_calls.load(Ordering::SeqCst), 2);
    assert_eq!(disabled_calls.load(Ordering::SeqCst), 1);
    // `build_proxy_state` was used above so the test is deterministic.
}

#[tokio::test]
async fn failover_disabled_never_tries_a_fresh_account_but_b1_retries_the_rejected_one() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"content-blocked","message":"blocked"}}"#;
    let (first, first_calls) = start_status_sequence_upstream(
        1,
        StatusCode::FORBIDDEN,
        SENSITIVE,
        r#"{"ok":"b1"}"#,
    )
    .await;
    let (backup, backup_calls) = start_status_sequence_upstream(
        0,
        StatusCode::FORBIDDEN,
        SENSITIVE,
        r#"{"ok":"backup"}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let first_id = create_proxy_api_credential_with_config(
        &pool,
        "first",
        &first,
        json!({
            "content_rejection_compat":"trim_old_tool_outputs",
            "failure_policy":{"retry_count":0,"retry_interval_ms":0,"failover_enabled":false}
        }),
    )
    .await;
    let backup_id = create_proxy_api_credential_with_config(
        &pool,
        "backup",
        &backup,
        json!({"failure_policy":{"retry_count":0,"retry_interval_ms":0}}),
    )
    .await;
    sqlx::query("UPDATE route_credentials SET route_priority = 1 WHERE id = ?")
        .bind(&first_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE route_credentials SET route_priority = 2 WHERE id = ?")
        .bind(&backup_id)
        .execute(&pool)
        .await
        .unwrap();
    RoutePoolRepository::replace_members(&pool, "codex", &[first_id, backup_id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-no-failover")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
            );
            headers
        },
        "/v1/chat/completions".parse().unwrap(),
        Body::from(b1_chat_body("gpt-5").to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(backup_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn precise_mode_b1_stays_on_the_pinned_account() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"sensitive_words_detected","message":"blocked"}}"#;
    let (pinned, pinned_calls) = start_status_sequence_upstream(
        1,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":"pinned"}"#,
    )
    .await;
    let (other, other_calls) = start_status_sequence_upstream(
        0,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":"other"}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let pinned_id = create_proxy_api_credential_with_config(
        &pool,
        "A",
        &pinned,
        json!({
            "content_rejection_compat":"trim_old_tool_outputs",
            "failure_policy":{"retry_count":0,"retry_interval_ms":0}
        }),
    )
    .await;
    let other_id = create_proxy_api_credential_with_config(
        &pool,
        "B",
        &other,
        json!({"failure_policy":{"retry_count":0,"retry_interval_ms":0}}),
    )
    .await;
    RoutePoolRepository::replace_members(&pool, "codex", &[pinned_id, other_id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-precise-b1")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
            );
            headers
        },
        "/v1/chat/completions".parse().unwrap(),
        Body::from(b1_chat_body("A/gpt-5").to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(pinned_calls.load(Ordering::SeqCst), 2);
    assert_eq!(other_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn content_rejection_without_a_trimmable_body_returns_the_pool_error() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"sensitive_words_detected","message":"blocked"}}"#;
    let (upstream, _calls) = start_status_sequence_upstream(
        usize::MAX,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":true}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let id = create_proxy_api_credential_with_config(
        &pool,
        "short",
        &upstream,
        json!({
            "content_rejection_compat":"trim_old_tool_outputs",
            "failure_policy":{"retry_count":0,"retry_interval_ms":0}
        }),
    )
    .await;
    RoutePoolRepository::replace_members(&pool, "codex", &[id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-short")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
            );
            headers
        },
        "/v1/chat/completions".parse().unwrap(),
        Body::from(json!({"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}).to_string()),
    )
    .await;

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("All route credentials failed"), "{body}");
    assert!(body.contains("blocked"), "{body}");
}

#[tokio::test]
async fn b1_runs_at_most_once_when_the_trimmed_request_is_rejected_again() {
    use crate::database::repositories::route_proxy_key_repository::RouteProxyKeyRepository;
    use crate::database::{create_memory_pool, run_migrations};

    const SENSITIVE: &str = r#"{"error":{"code":"sensitive_words_detected","message":"blocked"}}"#;
    let (upstream, calls) = start_status_sequence_upstream(
        usize::MAX,
        StatusCode::INTERNAL_SERVER_ERROR,
        SENSITIVE,
        r#"{"ok":true}"#,
    )
    .await;
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let id = create_proxy_api_credential_with_config(
        &pool,
        "still rejected",
        &upstream,
        json!({
            "content_rejection_compat":"trim_old_tool_outputs",
            "failure_policy":{"retry_count":0,"retry_interval_ms":0}
        }),
    )
    .await;
    RoutePoolRepository::replace_members(&pool, "codex", &[id])
        .await
        .unwrap();
    let route_key =
        RouteProxyKeyRepository::ensure_platform_key(&pool, "codex", "sk-ai-switch-test-b1-once")
            .await
            .unwrap();
    let runtime = RouteProxyRuntimeState::default();
    let state = build_proxy_state(pool.clone(), &runtime);
    let response = proxy_handler(
        AxumState(state),
        Method::POST,
        {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {route_key}")).unwrap(),
            );
            headers
        },
        "/v1/chat/completions".parse().unwrap(),
        Body::from(b1_chat_body("gpt-5").to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one original plus one trimmed attempt");
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("sensitive words detected"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection_compat_retries
```

Expected: 第一轮结束后没有 B1 第二轮，返回 502；`enabled_calls` 为 1 而不是 2。

- [x] **Step 3: 写最小实现**

在 `route_proxy_service.rs` 加入 import：

```rust
use crate::services::request_content_rejection;
```

在 `credential_indexes_by_priority` 附近加入纯选择函数：

```rust
fn content_rejection_retry_indexes(
    credentials: &[SelectedCredential],
    rejected_indexes: &[usize],
) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    rejected_indexes
        .iter()
        .copied()
        .filter(|index| *index < credentials.len())
        .filter(|index| seen.insert(*index))
        .filter(|index| {
            parse_json_object(&credentials[*index].config_json, "config")
                .ok()
                .is_some_and(|config| request_content_rejection::trim_enabled(&config))
        })
        .collect()
}
```

把当前单层重试循环重构为两轮。外层声明保持 `retry_errors`、`request_start`、`acquired_any`、`attempt`、各一次性 rewrite 标志不变，并加入：

```rust
let mut rejected_indexes = Vec::new();
let mut trimmed_content_rejection_retry = false;

'attempts: loop {
    let mut retry_queue = if trimmed_content_rejection_retry {
        content_rejection_retry_indexes(&credentials, &rejected_indexes)
            .into_iter()
            .map(|credential_index| (credential_index, 0usize))
            .collect::<VecDeque<_>>()
    } else {
        credential_indexes_by_priority(&credentials, cursor)
            .into_iter()
            .map(|credential_index| (credential_index, 0usize))
            .collect::<VecDeque<_>>()
    };

    while let Some((credential_index, credential_retry_count)) = retry_queue.pop_front() {
        // 现有循环体整体保持在这里。Task 4 的 A 分支 push rejected_indexes。
    }

    if trimmed_content_rejection_retry || rejected_indexes.is_empty() {
        break 'attempts;
    }

    let retry_indexes = content_rejection_retry_indexes(&credentials, &rejected_indexes);
    if retry_indexes.is_empty() {
        break 'attempts;
    }
    let Some(next_body) = request_content_rejection::elide_old_tool_outputs(&body_bytes) else {
        break 'attempts;
    };

    body_bytes = axum::body::Bytes::from(next_body);
    trimmed_content_rejection_retry = true;
}
```

关键约束：

```rust
// 第一轮仍使用 credential_indexes_by_priority，不改变聚合/精确/失败转移语义。
// 第二轮仅使用 content_rejection_retry_indexes，不包含未拒绝或未开启 B1 的账号。
// 第二轮 retry_count 从 0 开始，失败后仍走 A 分支；trimmed=true 阻止第三轮。
// body_bytes 只在第一轮全部结束后改写一次；成功候选、日志和上游请求均使用当前轮 body。
```

每次进入第二轮前不要手动清空 `retry_errors`。第二轮成功时原有成功返回路径生效；第二轮仍失败时沿用现有循环末尾的聚合错误，`acquired_any` 与最终 `All route credentials failed` 逻辑均不改动。B1 只是多一轮候选，不改变错误汇总格式。

- [x] **Step 4: 运行 B1 测试并确认通过**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib failover_disabled_never_tries
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib precise_mode_b1
```

Expected: A 只打一次后自然换号；关闭 failover 与精确模式都不碰新账号；启用 B1 的拒绝账号最多再打一次；trimmed 后仍拒绝不会出现第三次。

- [x] **Step 5: 跑代理回归测试并提交**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib route_proxy_service::tests::proxy_retries_retryable
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib context_length
```

Expected: 现有同号重试、请求过大直返和配额路径保持通过。

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/services/route_proxy_service.rs
git commit -m "feat(路由): 内容过滤后裁剪旧工具输出重试"
```

---

### Task 6: 增加账号级配置输入与后端校验

**Files:**
- Modify: `src-tauri/src/models/route_credential.rs`
- Modify: `src-tauri/src/services/route_credential_service.rs`
- Modify: `src-tauri/src/services/deeplink_service.rs`
- Modify: `src-tauri/src/services/route_config_service.rs`
- Modify: `src-tauri/src/services/route_recovery_service.rs`
- Modify: `src-tauri/src/services/route_relay_balance_service.rs`
- Modify: `src-tauri/src/web/router.rs`
- Modify: `src/lib/api/types.ts`

**Interfaces:**
- Consumes: `request_content_rejection::CONFIG_KEY`、`validate`。
- Produces: `CreateApiRouteCredentialInput.content_rejection_compat: Option<String>`，Serde 默认缺失为 `None`。
- Produces: TypeScript `content_rejection_compat?: "trim_old_tool_outputs" | "off" | null`。
- Produces: 创建时仅把 `trim_old_tool_outputs` 写入 `config_json`；缺失或 `off` 不写该键。
- Produces: 编辑时校验 `config_json.content_rejection_compat`，非法值在持久化前返回 `validation.content_rejection_compat`。

- [x] **Step 1: 写后端设置失败测试**

在 `route_credential_service.rs` 新增测试模块：

```rust
#[cfg(test)]
mod content_rejection_setting_tests {
    use super::*;
    use crate::database::{create_memory_pool, run_migrations};

    fn input(mode: Option<&str>) -> Value {
        let mut input = json!({
            "platform":"codex",
            "display_name":"Content rejection fixture",
            "api_key":"sk-test",
            "base_url":"https://example.com/v1",
            "interface_format":"openai",
            "model_mappings_json":"[]"
        });
        if let Some(mode) = mode {
            input["content_rejection_compat"] = json!(mode);
        }
        input
    }

    #[tokio::test]
    async fn content_rejection_compat_is_written_only_when_enabled() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        for (mode, expected) in [
            (None, None),
            (Some("off"), None),
            (Some("trim_old_tool_outputs"), Some("trim_old_tool_outputs")),
        ] {
            let row = RouteCredentialService::create_api(
                &pool,
                serde_json::from_value(input(mode)).unwrap(),
            )
            .await
            .unwrap();
            let config: Value = serde_json::from_str(&row.config_json).unwrap();
            assert_eq!(
                config
                    .get(crate::services::request_content_rejection::CONFIG_KEY)
                    .and_then(Value::as_str),
                expected
            );
        }
    }

    #[tokio::test]
    async fn invalid_content_rejection_compat_creation_is_rejected_before_persistence() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        let error = RouteCredentialService::create_api(
            &pool,
            serde_json::from_value(input(Some("trim_all"))).unwrap(),
        )
        .await
        .expect_err("invalid mode");
        assert!(matches!(
            error,
            AppError::Validation {
                code: "validation.content_rejection_compat",
                ..
            }
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_credentials")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn invalid_content_rejection_compat_update_preserves_saved_value() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        let row = RouteCredentialService::create_api(
            &pool,
            serde_json::from_value(input(Some("trim_old_tool_outputs"))).unwrap(),
        )
        .await
        .unwrap();
        let invalid = json!({
            "display_name": row.display_name,
            "email": row.email,
            "status": row.status,
            "route_priority": row.route_priority,
            "max_concurrency": row.max_concurrency,
            "secret_payload_json": row.secret_payload_json,
            "config_json": r#"{"content_rejection_compat":"trim_all","keep":"yes"}"#,
            "preview_json": row.preview_json
        });
        assert!(RouteCredentialService::update(
            &pool,
            row.id.clone(),
            serde_json::from_value(invalid).unwrap()
        )
        .await
        .is_err());
        let stored = RouteCredentialRepository::get(&pool, &row.id).await.unwrap();
        let config: Value = serde_json::from_str(&stored.config_json).unwrap();
        assert_eq!(
            config["content_rejection_compat"],
            "trim_old_tool_outputs"
        );
    }

    #[tokio::test]
    async fn turning_the_setting_off_removes_it_from_config() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        let row = RouteCredentialService::create_api(
            &pool,
            serde_json::from_value(input(Some("trim_old_tool_outputs"))).unwrap(),
        )
        .await
        .unwrap();
        let update = json!({
            "display_name": row.display_name,
            "email": row.email,
            "status": row.status,
            "route_priority": row.route_priority,
            "max_concurrency": row.max_concurrency,
            "secret_payload_json": row.secret_payload_json,
            "config_json": r#"{"keep":"yes"}"#,
            "preview_json": row.preview_json
        });
        let updated = RouteCredentialService::update(
            &pool,
            row.id,
            serde_json::from_value(update).unwrap(),
        )
        .await
        .unwrap();
        let config: Value = serde_json::from_str(&updated.config_json).unwrap();
        assert!(config.get("content_rejection_compat").is_none());
        assert_eq!(config["keep"], "yes");
    }
}
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection_setting
```

Expected: 编译失败，提示 `CreateApiRouteCredentialInput` 没有 `content_rejection_compat` 字段。

- [x] **Step 3: 增加模型和 TypeScript 输入字段**

在 `CreateApiRouteCredentialInput` 的 `force_reasoning_content` 旁加入：

```rust
/// 账号级内容过滤兼容。只允许 `trim_old_tool_outputs`；缺失或 `off`
/// 表示关闭，编辑时由客户端删除该键。
#[serde(default)]
pub content_rejection_compat: Option<String>,
```

在 `src/lib/api/types.ts` 的同一输入类型中加入：

```ts
content_rejection_compat?: "trim_old_tool_outputs" | "off" | null;
```

- [x] **Step 4: 写服务校验与写入实现**

在 `create_api` 的 `request_compression::validate` 之后加入：

```rust
crate::services::request_content_rejection::validate(
    input
        .content_rejection_compat
        .as_ref()
        .map(|mode| json!(mode))
        .as_ref(),
)?;
```

在写 `request_brotli_compression` 的代码之后加入：

```rust
if let Some(mode) = input
    .content_rejection_compat
    .as_deref()
    .filter(|mode| *mode != "off")
{
    config[crate::services::request_content_rejection::CONFIG_KEY] = json!(mode);
}
```

在 `update` 的 `request_compression::validate` 之后加入：

```rust
crate::services::request_content_rejection::validate(
    config.get(crate::services::request_content_rejection::CONFIG_KEY),
)?;
```

运行编译，找到所有 Rust 结构体字面量缺失字段：

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo check
```

对每个 `CreateApiRouteCredentialInput` 结构体字面量测试构造器补：

```rust
content_rejection_compat: None,
```

仓库中共有 27 个 `CreateApiRouteCredentialInput` 结构体字面量。按编译器报错逐项在 `force_reasoning_content` 后补同一缩进的 `content_rejection_compat: None`；其中 `deeplink_service.rs` 等非测试构造器也必须补。完成后重新运行 `cargo check`，直到没有 missing-field 错误。

- [x] **Step 5: 运行设置测试与编译检查**

Run:

```bash
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib content_rejection_setting
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo check
pnpm typecheck
```

Expected: 设置测试通过；`cargo check` 无缺失字段错误；TypeScript 输入字段可通过类型检查。

- [x] **Step 6: 格式化并提交**

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/models/route_credential.rs src-tauri/src/services/route_credential_service.rs src-tauri/src/services/deeplink_service.rs src-tauri/src/services/route_config_service.rs src-tauri/src/services/route_recovery_service.rs src-tauri/src/services/route_relay_balance_service.rs src-tauri/src/web/router.rs src/lib/api/types.ts
git commit -m "feat(路由): 增加账号级内容过滤兼容配置"
```

---

### Task 7: 增加账号编辑 UI 与失败提示

**Files:**
- Modify: `src/screens/AccountsScreen.tsx`
- Test: `tests/AccountsScreen.test.tsx`

**Interfaces:**
- Consumes: `CreateApiRouteCredentialInput.content_rejection_compat`。
- Consumes: `config_json.content_rejection_compat = "trim_old_tool_outputs"`。
- Produces: 创建/编辑高级页复选框 `内容过滤兼容`。
- Produces: `ContentRejectionCompatOption({ checked, onChange })`。
- Produces: `contentRejectionCompatFromConfig(config): boolean`。
- Produces: `apiConfigJsonWithFields` 新增最后一个可选参数 `contentRejectionCompat = false`，开启写键，关闭删除键。

- [x] **Step 1: 写 UI 失败测试**

在 `tests/AccountsScreen.test.tsx` 加入：

```tsx
it("writes content rejection compatibility when creating an API account", async () => {
  renderScreen();

  await userEvent.click(await screen.findByRole("button", { name: "新增账号" }));
  await userEvent.click(screen.getByRole("button", { name: "API 账号" }));
  await userEvent.type(screen.getByLabelText("API 账号名称"), "Compat API");
  await userEvent.type(screen.getByLabelText("API Key"), "sk-compat");
  await openFormTab("高级");
  await userEvent.click(screen.getByLabelText("内容过滤兼容"));
  await userEvent.click(screen.getByRole("button", { name: "保存账号" }));

  await waitFor(() =>
    expect(createApiRouteCredential).toHaveBeenCalledWith(
      expect.objectContaining({
        display_name: "Compat API",
        content_rejection_compat: "trim_old_tool_outputs",
      }),
    ),
  );
});

it("removes content rejection compatibility from config when edited off", async () => {
  const api = {
    ...credentialsFixture[1],
    platform: "codex" as const,
    display_name: "Compat Edit",
    config_json: JSON.stringify({
      base_url: "https://api.upstream.test/v1",
      interface_format: "openai",
      model_mappings: [],
      content_rejection_compat: "trim_old_tool_outputs",
    }),
  };
  vi.mocked(listRouteCredentials).mockResolvedValue([api]);
  vi.mocked(updateRouteCredential).mockResolvedValue(api);

  renderScreen();
  await userEvent.click(await screen.findByRole("button", { name: "编辑 Compat Edit" }));
  await openFormTab("高级");
  const toggle = screen.getByLabelText("内容过滤兼容");
  expect(toggle).toBeChecked();
  await userEvent.click(toggle);
  await userEvent.click(screen.getByRole("button", { name: "保存修改" }));

  await waitFor(() => expect(updateRouteCredential).toHaveBeenCalled());
  const config = JSON.parse(vi.mocked(updateRouteCredential).mock.calls[0][1].config_json);
  expect(config.content_rejection_compat).toBeUndefined();
});

it("labels content rejection and explains content-blocked with the B1 state", async () => {
  vi.mocked(listRouteCredentials).mockResolvedValue([
    {
      ...credentialsFixture[1],
      id: "cred-b1-on",
      display_name: "B1 Enabled",
      status: "error",
      last_failure_kind: "content_rejected",
      last_failure_message: "content-blocked",
      last_failure_response_json:
        '{"error":{"code":"content-blocked","message":"blocked by edge"}}',
      config_json: JSON.stringify({ content_rejection_compat: "trim_old_tool_outputs" }),
    },
    {
      ...credentialsFixture[0],
      id: "cred-b1-off",
      display_name: "B1 Disabled",
      status: "error",
      last_failure_kind: "content_rejected",
      last_failure_message: "sensitive_words_detected",
      last_failure_response_json:
        '{"error":{"code":"sensitive_words_detected","message":"blocked"}}',
    },
  ]);

  renderScreen();

  const firstRow = await screen.findByLabelText("放置在 B1 Enabled 前");
  const secondRow = screen.getByLabelText("放置在 B1 Disabled 前");
  expect(within(firstRow).getByText("失败类型：内容被上游过滤")).toBeInTheDocument();
  expect(within(firstRow).getByText(/已尝试省略旧工具输出后重试/)).toBeInTheDocument();
  expect(within(secondRow).getByText(/可在账号编辑中开启「内容过滤兼容」/)).toBeInTheDocument();
});
```

- [x] **Step 2: 运行测试并确认失败**

Run:

```bash
pnpm test:run tests/AccountsScreen.test.tsx -t "content rejection compatibility|content rejection"
```

Expected: 找不到“内容过滤兼容”复选框；创建请求缺少字段；提示仍只识别 `sensitive_words_detected`。

- [x] **Step 3: 写最小 UI 实现**

在状态声明区加入：

```tsx
const [apiContentRejectionCompat, setApiContentRejectionCompat] = useState(false);
const [editContentRejectionCompat, setEditContentRejectionCompat] = useState(false);
```

在 `resetCreateForm` 加入 `setApiContentRejectionCompat(false);`。编辑抽屉 hydrate 的 API 分支加入：

```tsx
setEditContentRejectionCompat(contentRejectionCompatFromConfig(config));
```

官方账号分支加入 `setEditContentRejectionCompat(false);`。

在 `forceReasoningContentFromConfig` 旁加入：

```tsx
function contentRejectionCompatFromConfig(config: Record<string, unknown>): boolean {
  return config.content_rejection_compat === "trim_old_tool_outputs";
}

function writeContentRejectionCompat(config: Record<string, unknown>, enabled: boolean) {
  if (enabled) {
    config.content_rejection_compat = "trim_old_tool_outputs";
  } else {
    delete config.content_rejection_compat;
  }
}
```

在 `apiConfigJsonWithFields` 参数列表末尾加入：

```tsx
contentRejectionCompat = false,
```

并在 `force_reasoning_content` 写入逻辑旁加入：

```tsx
writeContentRejectionCompat(config, contentRejectionCompat);
```

创建请求 `input` 加入：

```tsx
...(apiContentRejectionCompat
  ? { content_rejection_compat: "trim_old_tool_outputs" as const }
  : {}),
```

编辑保存调用 `apiConfigJsonWithFields` 时在 `editRequestCompressionMode` 后传入：

```tsx
editContentRejectionCompat,
```

新增可复用控件，放在 `ResponsesEncryptedContentCleanupOption` 附近：

```tsx
function ContentRejectionCompatOption({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: (enabled: boolean) => void;
}) {
  return (
    <label className="flex items-start gap-2 rounded-xl border border-stone-200 bg-white px-3 py-2 text-[12px] font-medium text-stone-700">
      <input
        aria-label="内容过滤兼容"
        checked={checked}
        className="mt-0.5"
        onChange={(event) => onChange(event.target.checked)}
        type="checkbox"
      />
      <span className="grid gap-1">
        <span>内容过滤兼容</span>
        <span className="text-[11px] font-medium text-stone-500">
          上游因敏感词拒绝请求时，省略较旧的工具输出并重试一次。只影响发往该上游的请求，不改本地会话记录。默认关闭；请求压缩不能绕过内容过滤。
        </span>
      </span>
    </label>
  );
}
```

在创建与编辑高级页各渲染一次：

```tsx
<ContentRejectionCompatOption
  checked={apiContentRejectionCompat}
  onChange={setApiContentRejectionCompat}
/>
```

```tsx
<ContentRejectionCompatOption
  checked={editContentRejectionCompat}
  onChange={setEditContentRejectionCompat}
/>
```

更新提示识别：

```tsx
const CONTENT_REJECTION_ERROR_CODES = ["sensitive_words_detected", "content-blocked"];
const contentRejectionCompatEnabled =
  contentRejectionCompatFromConfig(parseJsonObject(credential.config_json));
const contentRejectionCompatHint = contentRejectionCompatEnabled
  ? "该账号已开启内容过滤兼容：ai-switch 已尝试省略旧工具输出后重试。"
  : "可在账号编辑中开启「内容过滤兼容」。";
const contentRejectedByCode = [response, credential.last_failure_message].some((value) => {
  const text = value?.toLowerCase();
  return CONTENT_REJECTION_ERROR_CODES.some((code) => text?.includes(code));
});
const contentRejected =
  credential.last_failure_kind === "content_rejected" || contentRejectedByCode;
```

提示面板中把失败类型显示改为：

```tsx
失败类型：{contentRejected
  ? "内容被上游过滤"
  : credential.last_failure_kind?.trim() || "未知"}
```

保留原敏感词提醒；另在 `contentRejected` 为真时、且不受 `sensitiveWords` 条件限制地按开关追加：

```tsx
{contentRejected ? (
  <span
    className="mt-1 block"
    data-testid={`credential-content-rejection-compat-${credential.id}`}
  >
    {contentRejectionCompatHint}
  </span>
) : null}
```

- [x] **Step 4: 运行 UI 测试并确认通过**

Run:

```bash
pnpm test:run tests/AccountsScreen.test.tsx -t "content rejection compatibility|content rejection"
pnpm typecheck
```

Expected: 三个新测试通过；现有敏感词测试、编辑保存与账号创建测试继续通过。

- [x] **Step 5: 格式化并提交**

```bash
pnpm exec prettier --write src/screens/AccountsScreen.tsx tests/AccountsScreen.test.tsx
git add src/screens/AccountsScreen.tsx tests/AccountsScreen.test.tsx
git commit -m "feat(账号): 增加内容过滤兼容开关与提示"
```

---

### Task 8: 更新中英文文档并执行全量验证

**Files:**
- Modify: `docs-site/docs/guide/reliability.md`
- Modify: `docs-site/docs/en/guide/reliability.md`

**Interfaces:**
- Consumes: 最终行为与 UI 文案。
- Produces: 中文/英文用户文档，明确 A、B1、账号级开关、只保留最新 6 条工具输出、Brotli 无效。

- [x] **Step 1: 更新中文“敏感词检测提醒”章节**

把本节扩写为以下内容，并保持现有提醒原文不变：

```markdown
### 敏感词检测提醒

如果 `last_failure_response_json` 或 `last_failure_message` 里出现
`sensitive_words_detected` 或 `content-blocked`，状态标签的悬停提示里会多出一条友情提醒：

> 友情提醒：当前中转站似乎对项目存在关键词检测，您的项目可能存在敏感词，也不排除是中转站误判。

这类错误来自中转站自己的内容过滤，不是账号坏了。ai-switch 会把它标成
`content_rejected`：不会在同一个账号上重复发送同一份请求，不会增加失败次数，
不会冷却账号，也不会改账号状态。聚合模式且故障转移开启时，请求会自然尝试池里的
下一个账号；精确模式或关闭故障转移时不会换号。

如果账号在编辑抽屉的高级页开启「内容过滤兼容」，第一轮候选全部结束后，
ai-switch 会省略较早的工具输出并重试一次：

- 保留最新 6 条工具输出，只替换更早的、正文超过 128 字节的纯文本输出；
- 不改用户消息、系统提示、工具定义、工具参数、`call_id` 或 reasoning；
- 图片、文件和其它非文本结构不处理；
- 只重试第一轮中“被内容过滤拒绝且开启该开关”的账号，最多一次；
- 不改本地会话记录，下一次客户端请求仍会发送原始历史。

开关是账号级的，因为不同中转站的词表与兼容行为不同。触发词如果就在最新 6 条
工具输出、用户消息或系统提示里，裁剪旧工具输出不会解决问题，ai-switch 会保留
上游的原始拒绝响应。

**Brotli、Gzip、Zstd 请求压缩不能解决这个问题。** New API 系中转站会先解压
请求，再对合并后的消息、instructions、tools 与工具输出做关键词扫描；实测同一份
含触发词的请求无论明文还是 Brotli 都返回 `sensitive_words_detected`。
```

- [x] **Step 2: 更新中文失败分类表**

在 `last_failure_kind` 表中加入：

```markdown
| `content_rejected` | 上游按内容策略拒绝请求；只写提示，不增加失败次数、不冷却、不改账号状态 | 不归账号或模型健康 |
```

- [x] **Step 3: 更新英文对应章节**

在 `docs-site/docs/en/guide/reliability.md` 写入等价英文内容：

```markdown
### Sensitive-content rejection reminder

When `last_failure_response_json` or `last_failure_message` contains
`sensitive_words_detected` or `content-blocked`, the status tag's hover panel gains
the existing keyword-filter reminder and classifies the failure as `content_rejected`.

A content rejection does not mean the credential is unhealthy. ai-switch does not
resend the identical request to the same account, does not increase the failure
count, does not start a cooldown, and does not change the account status. In
aggregate mode with failover enabled, the existing candidate queue naturally moves
to the next account. Precise mode and accounts with failover disabled do not switch.

When **Content-filter compatibility** is enabled on the account's Advanced tab,
ai-switch waits until the first candidate pass is exhausted, then omits older tool
outputs and retries once:

- the latest six tool outputs are preserved; only older plain-text outputs larger
  than 128 bytes are replaced;
- user messages, system prompts, tool definitions, tool arguments, `call_id`, and
  reasoning are never modified;
- images, files, and other non-text structures are left untouched;
- the second pass only targets accounts that both rejected the content and enabled
  the option, and there is never a third pass;
- local conversation storage is unchanged, so the next client request starts from
  the original history.

The option is account-scoped because upstream word lists differ. If the trigger is
inside the latest six tool outputs, a user message, or system instructions, B1 cannot
help and the upstream's original rejection is returned.

**Brotli, Gzip, and Zstd request compression do not solve this problem.** New API
gateways decompress the request first, then scan the combined messages,
instructions, tools, and tool outputs. The same request is rejected with or without
Brotli in live tests.
```

英文分类表加入：

```markdown
| `content_rejected` | The upstream rejected the request under a content policy; this only records a hint and does not change retry count, cooldown, or account status | Neither account nor model health |
```

- [x] **Step 4: 运行仓库全量验证**

Run:

```bash
pnpm typecheck
pnpm test:run
pnpm release:manifest:test
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo test --lib
cd src-tauri && CARGO_TARGET_DIR=target-codex cargo check
```

Expected: 所有命令退出码为 0；Rust 输出中没有 failed test；不产生除 `src-tauri/target-codex/` 外的构建目录。

- [x] **Step 5: 检查工作区与 diff**

Run:

```bash
git status --short
git diff --check
git diff --stat
```

Expected: 只有两份 reliability 文档待提交；`git diff --check` 无输出。

- [x] **Step 6: 提交文档**

```bash
git add docs-site/docs/guide/reliability.md docs-site/docs/en/guide/reliability.md
git commit -m "docs(路由): 说明内容过滤兼容与压缩限制"
```

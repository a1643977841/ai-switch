# 内容过滤拒绝兼容（A + B1）设计

日期：2026-10-20
状态：设计已确认，待实现
范围：路由代理（`src-tauri/src/services/route_proxy_service.rs`）与账号配置

## 1. 背景

本机日志中存在大量由中转站返回的内容过滤错误，错误码为
`sensitive_words_detected`，主要来自 AgentRouter（New API 系）。

实测证据：

- `~/.ai-switch/ai-switch.db` 中共 977 条 `sensitive_words_detected`；
- 2026-10-19 11:32–11:40Z 一段会话连续产生 100 条同类失败，均是同一份
  上下文、同一账号 `deepseek-v4-flash`，每条按 `retry_count = 2` 重试三次；
- 触发词不在用户当前提问里，而在很早以前的一次工具输出中：
  `Our Free Model` 前端 bundle 的键名 `tag.eacChannel`，其中子串 `cch`
  命中站点的关键词表（大小写不敏感）；
- 把 `cch` 从该 40 KB 片段中删除后，同一请求立即返回 `200`；
- 同一个触发请求改用 `Content-Encoding: br` 发送，仍然
  `sensitive_words_detected`，因为 New API 在
  `middleware.DecompressRequestMiddleware` 中先解压，再对
  `CombineText` 做 Aho-Corasick 子串扫描。

结论：

1. 这是**内容级拒绝**，不是账号、网络或模型故障；
2. 同一份 body 在同一账号上重试没有任何意义；
3. Brotli/Gzip/Zstd 等传输压缩不能绕过，因为扫描发生在解压之后。

## 2. 目标与非目标

### 目标

1. 内容拒绝不再消耗账号的重试预算，不增加失败计数，不触发冷却，不改变账号
   `status`；
2. 保留原始响应供 UI 显示敏感词提醒；
3. 聚合模式且故障转移开启时，仍由现有账号队列完成换号；
4. 账号级可选开启 B1：全部候选都失败后，省略较旧的工具输出并重试一次；
5. 精确模式不换号，只允许同账号的 B1 重试；
6. 行为可解释、可测试，且不改变本地会话存储。

### 非目标

- 不做零宽字符、同形字、编码混淆等规避上游内容过滤的手段；
- 不尝试通过 Brotli/Gzip/分块编码绕过关键词扫描；
- 不修改用户消息、系统提示、当前轮代码或工具调用参数；
- 不做动态关键词探测，不额外发送探测请求；
- 不引入跨请求的“已裁剪”记忆；v1 每次命中后独立重试一次。

## 3. 术语

- **内容拒绝（content rejection）**：上游以明确的错误码表示请求文本命中其
  内容过滤规则。当前识别：
  - `sensitive_words_detected`（New API 系）
  - `content-blocked`（部分网关的边缘过滤）
- **A**：把内容拒绝与账号健康故障分流。
- **B1**：内容拒绝后省略较旧工具输出，重写请求体并重试一次。

## 4. 关键约束：换号只在聚合模式发生

现有路由语义必须保持：

| 模式 / 配置 | 第一轮候选 |
| --- | --- |
| 聚合 + `failover_enabled = true` | 池内全部可用账号 |
| 聚合 + `failover_enabled = false` | 仅当前账号 |
| 精确模式 | 仅模型名前缀指定的账号 |

因此：

- A **不主动换号**，只负责不重试当前账号并继续现有队列；
- 聚合模式队列里还有账号时，自然换号；
- 精确模式下队列只有一个账号，A 结束后进入 B1 判断；
- B1 的第二轮是“改写请求后重试”，不是故障转移，精确模式同样允许。

## 5. A：识别与分流

### 5.1 识别函数

`response_failure_service.rs` 新增：

```rust
const CONTENT_REJECTION_CODES: &[&str] = &[
    "sensitive_words_detected",
    "content-blocked",
];

pub(crate) fn is_content_rejection(failure: &SemanticResponseFailure) -> bool {
    failure.code.as_deref().is_some_and(|code| {
        CONTENT_REJECTION_CODES
            .iter()
            .any(|known| code.trim().eq_ignore_ascii_case(known))
    })
}
```

只匹配 `error.code` 的完整值，不匹配消息文本，避免把普通 500 误判为内容拒绝。

### 5.2 新失败类型

`last_failure_kind` 新增取值 `content_rejected`。

`route_failure_scope.rs` 显式声明：

```rust
"content_rejected" => false, // 内容级拒绝，不是账号/网络故障
```

虽然内容拒绝不会走健康记账，显式分支可防止 `_ => true` 的兜底把它误判成账号级。

### 5.3 只记提示，不记健康失败

`RouteCredentialRepository` 新增：

```rust
pub async fn record_content_rejection(
    pool: &SqlitePool,
    id: &str,
    message: &str,
    response_body: Option<&[u8]>,
) -> Result<(), AppError>
```

只更新：

```sql
last_failure_kind = 'content_rejected',
last_failure_message = ?,
last_failure_response_json = ?,
updated_at = ?
```

**不动**：

- `transient_failure_count`
- `next_retry_at`
- `cooldown_until`
- `semantic_failure_streak_count`
- `semantic_failure_streak_fingerprint`
- `status`

为避免覆盖仍在生效的其它故障信息，写入前检查：

```sql
cooldown_until IS NULL OR cooldown_until <= now
```

若账号正因其它原因冷却，则保留原有 `last_failure_*`，但仍按内容拒绝处理路由。
该检查在同一个事务内完成，避免读写竞争。

### 5.4 代理分支

在 `route_proxy_service.rs` 的 `semantic_failure` 分支之前插入：

```text
if is_content_rejection(failure):
    record_content_rejection(...)          // 不动健康
    notify_status_change(...)              // 刷新 UI 提示
    rejected_indexes.push(credential_index)
    retry_errors.push("<账号>: <原始消息>")
    stop_failover_when_disabled(&mut retry_queue, failover_enabled)
    continue                               // 不 push_front，不重试同账号
```

要点：

- 不增加 `credential_retry_count`；
- 不消耗 `failure_policy.retry_count`；
- 聚合模式下队列自然前进到下一个账号；
- 精确模式下 `continue` 后队列为空，进入 B1 判断。

### 5.5 SSE 首帧失败

若上游以 `200 + response.failed` 返回内容拒绝，
`handle_stream_prime_failure` 同样按 A 处理：

- 记录 `content_rejected`；
- `retry_same_credential = false`；
- 不记 `"transport"`；
- 把拒绝账号加入 `rejected_indexes`，由外层决定是否 B1。

## 6. B1：账号级“裁剪旧工具输出后重试一次”

### 6.1 配置

账号 `config_json` 新增：

```json
{ "content_rejection_compat": "trim_old_tool_outputs" }
```

- 缺失或 `"off"`：关闭；
- `"trim_old_tool_outputs"`：开启；
- 其它值：创建或更新时返回校验错误。

开关是**账号级**：聚合模式下不同账号可以有不同的上游词表与兼容策略；
精确模式本身就是按账号寻址。

### 6.2 模块

新建 `src-tauri/src/services/request_content_rejection.rs`：

```rust
pub(crate) const CONFIG_KEY: &str = "content_rejection_compat";

pub(crate) fn validate(mode: Option<&Value>) -> Result<(), AppError>;
pub(crate) fn trim_enabled(config: &Value) -> bool;
pub(crate) fn elide_old_tool_outputs(body: &[u8]) -> Option<Vec<u8>>;
```

`services/mod.rs` 注册该模块。

### 6.3 裁剪规则

**Responses 格式**：处理顶层 `input[]` 中：

- `type = "function_call_output"`
- `type = "custom_tool_call_output"`
- `type = "local_shell_call_output"`

**Chat Completions 格式**：处理 `messages[]` 中 `role = "tool"` 的 `content`。

统一规则：

1. 按出现顺序收集所有候选工具输出；
2. **保留最新 6 条**原样不动；
3. 其余较旧的输出：
   - 字符串 → 替换为固定占位符；
   - 仅含文本块的数组 → 替换为单个文本块；
   - 含图片、文件或其它非文本结构 → 不碰；
4. 只处理长度超过 128 字节的输出；
5. 至少修改一条才返回 `Some`；否则返回 `None`（幂等）。

占位符：

```text
[ai-switch: older tool output omitted after upstream content filter; re-run the command if needed]
```

**绝不修改**：

- user / assistant / system / developer 消息
- `instructions`
- `tools`
- `function_call.arguments`
- `call_id` / `tool_call_id`
- reasoning / encrypted content

### 6.4 两轮流程

现有 `while let Some(...) = retry_queue.pop_front()` 外面包一层
`'attempts: loop`：

```text
let mut body_bytes = 原始请求;
let mut rejected_indexes = Vec::new();
let mut trimmed = false;

'attempts: loop {
    let mut retry_queue = if trimmed {
        // 第二轮：只重试“内容拒绝过且启用 B1”的账号
        候选中的对应账号
    } else {
        // 第一轮：沿用现有队列
        credential_indexes_by_priority(...)
    };

    while let Some((index, retry_count)) = retry_queue.pop_front() {
        // 用当前 body_bytes 构建并发送请求
        // 内容拒绝 → A 分支，记录 rejected_indexes
        // 其它失败 → 现有逻辑不变
    }

    if trimmed || rejected_indexes.is_empty() {
        break;
    }

    let eligible: Vec<usize> = rejected_indexes
        .iter()
        .copied()
        .filter(|index| trim_enabled(config_of(credentials[*index])))
        .collect();

    if eligible.is_empty() {
        break;
    }

    let Some(next) = elide_old_tool_outputs(&body_bytes) else {
        break;
    };

    body_bytes = next.into();
    trimmed = true;
    continue 'attempts;
}
```

要点：

- 第一轮 body 不变；
- 第二轮 body 只裁剪一次；
- 第二轮只打“拒绝过且启用 B1”的账号；
- 第二轮不会再次进入第三轮；
- `attempt` 继续累加，日志可区分发送次数；
- 现有的 `acquired_any` 与最终聚合错误逻辑保持不变。

### 6.5 并发与租约

重试同一账号时仍需重新获取 `activity_lease`，与现有循环一致；
不得跨轮次持有租约。

## 7. UI 与提示

### 7.1 账号编辑抽屉

创建与编辑 API 账号时各增加一个复选框：

> **内容过滤兼容**
> 上游因敏感词拒绝请求时，省略较旧的工具输出并重试一次。只影响发往该上游的
> 请求，不改本地会话记录。默认关闭。

写入/删除 `config.content_rejection_compat` 的规则与现有
`force_reasoning_content` 一致：关闭时删除键，不写 `false`。

### 7.2 失败提示

`CredentialFailureTooltip`：

- `last_failure_kind === "content_rejected"` 显示“内容被上游过滤”；
- 敏感词提示同时识别 `sensitive_words_detected` 与 `content-blocked`；
- 已开启 B1 时追加：“ai-switch 已尝试省略旧工具输出后重试。”
- 未开启时追加：“可在账号编辑中开启「内容过滤兼容」。”

注意：成功请求后 `clear_transient_failure` 会清掉 `last_failure_*`，提示自然消失。

## 8. 后端输入与校验

`CreateApiRouteCredentialInput` 新增：

```rust
#[serde(default)]
pub content_rejection_compat: Option<String>,
```

- `create_api` 调用 `request_content_rejection::validate`；
- 非 `off` 时写入 `config`；
- `update` 在解析 `config_json` 时补一行校验；
- `src/lib/api/types.ts` 增加同名字段。

导出/迁移与现有兼容开关保持一致：v1 不携带该配置。

## 9. 测试计划

### 9.1 识别

- `sensitive_words_detected` → true
- `content-blocked` → true
- 随机 500、`upstream_status`、`insufficient_user_quota` → false

### 9.2 作用域

- `content_rejected` 不是账号级失败

### 9.3 裁剪器

- 7 条 Responses 工具输出 → 只改最旧 1 条
- 6 条 → 返回 `None`
- 保留 `call_id`、`type`、数组结构合法
- user / assistant / instructions / tools / arguments 不变
- Chat `role: tool` 同样生效
- 第二次调用返回 `None`（幂等）
- 非 JSON body → `None`
- 含图片的工具输出不修改

### 9.4 代理集成（fake upstream）

- 聚合 + failover 开：A 拒绝 → 只打一次 A，立即 B 成功；不裁剪
- 聚合 + failover 关：A 拒绝 → 不换号；A 启用 B1 则裁剪后重试 A
- 精确模式：A 拒绝 → 不换号；A 启用 B1 则裁剪后重试 A
- 聚合全拒 + B1：第二轮只打“拒绝且启用 B1”的账号
- 聚合全拒 + 无 B1：直接返回聚合错误
- 裁剪后仍拒绝：不再进入第三轮
- 所有内容拒绝场景：`transient_failure_count`、`status`、`cooldown_until` 不变
- 账号正因其它原因冷却时：不覆盖原有 `last_failure_*`

### 9.5 仓库

- `record_content_rejection` 只写 `last_failure_*` 与 `updated_at`
- 冷却中的账号不被覆盖
- 账号不存在时返回 `validation.route_credential_not_found`

### 9.6 UI

- `content_rejected` 显示中文标签
- 敏感词提示在已开启/未开启时文案不同
- 关闭 B1 后 `config` 中不残留该键

## 10. 已知限制

- 触发词在最新 6 条工具输出里时，B1 无效，正常返回错误；
- 只处理工具输出，不改用户输入、系统提示或代码正文；
- 每轮最多一次裁剪重试；
- 若同一会话的旧工具输出反复触发，每个新回合都会先被拒一次再裁剪；
- 不做跨请求记忆，避免未经确认就改变后续上下文。

## 11. 被排除的方案

| 方案 | 结论 |
| --- | --- |
| Brotli/Gzip/Zstd 请求压缩 | 无效：上游先解压再扫描 |
| 零宽字符、同形字、编码混淆 | 不稳定，破坏代码与工具调用，可能违反上游条款 |
| 自动改写用户消息或系统提示 | 风险过高，可能改变任务语义 |
| 把敏感词错误伪装成 `context_length_exceeded` 触发压缩 | 语义不诚实，不采用 |
| 池级 B1 开关 | 不同账号上游能力不同，采用账号级 |

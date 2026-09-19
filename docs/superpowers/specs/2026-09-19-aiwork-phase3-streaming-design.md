# AI Work Assistant Phase 3A：流式 Lease 生命周期设计

## 1. 目标与阶段边界

本阶段把已经验证的 Core 用户额度预占、upstream lease 和非流式 Chat 执行扩展到流式出口。目标是让 OpenAI Chat、OpenAI Responses 和 Anthropic Messages 的流式请求在同一条 Core 权威链路中完成鉴权、预算预占、账号 lease、事件转发、heartbeat、断流/取消、保守结算、审计和重启恢复。

本阶段不迁移素材所有权、不建立视频 jobs/attempts、不拆分独立 Local Agent，也不改变旧模式（`core_mode=off` 或未接入的 shadow 路径）的行为。素材和视频分别进入后续 Phase 3B/3C 规格。

真实 Trae/WorkBuddy 流式适配器只有在接口和 Mock contract tests 通过后才接入；未注册流式适配器时，Core enforce 必须返回稳定的 `scheduler_endpoint_not_enabled`，绝不回退到 `ApiPool`。

## 2. 已确认的现状

- `src-core` 已有 `requests`、用户 quota reservation、`upstream_leases`、`heartbeat_upstream_lease`、`settle_upstream_lease` 和 `recover_expired_upstream_leases`。
- 当前 Core `RequestState` 没有取消请求/取消完成状态；upstream settlement 只有成功、明确拒绝、未知三类结果。
- `CoreUpstreamExecutor` 和 `LeaseUpstreamAdapter` 当前只覆盖非流式 `ChatExecutionRequest`。
- Tauri 旧流式入口在 `routes.rs` 与 `wb_route.rs` 中直接取 `ApiPool` 账号并维护进程内 inflight/日志；Core enforce 当前对流式路径 fail-closed。
- 现有 SSE 转换器可以产生 OpenAI/Anthropic 协议片段，但其上游读取、账号选择和结算责任仍属于旧路径。

## 3. 不变量

1. Core Principal 的 `user_id` 是唯一业务所有者；请求体中的 `user_id`、`account_ref`、Key 约束和协议字段不能改变 Core 选择。
2. 一个新请求最多创建一个用户 reservation 和一个 upstream lease；相同幂等键不得创建第二个流或第二次上游执行。
3. lease 未确认成功前不能释放未知结果。客户端断开不等于上游取消成功。
4. 只有 Core 返回 `Acquired(UpstreamLeaseGrant)` 时适配器才能执行；Replay 只返回持久化状态，不含可执行 lease grant。
5. heartbeat 只延长仍处于 `held`/`active` 的 lease，并且必须经过请求所有者校验；终态 lease 的 heartbeat 无副作用。
6. 已发送任意上游事件后发生断流、取消失败、读取异常或进程退出，结果必须进入 `unknown`，reservation 保持 `unknown`，等待对账。
7. 明确收到上游拒绝且未接受请求时释放用户 reservation 和 lease；确认取消成功时也释放；无法确认取消时进入 `unknown`。
8. 协议转换失败、发送通道关闭、上游响应没有终止信号或 usage 不可信时，不得伪造成功或 actual usage。
9. 所有持久化错误分类必须经过 allowlist；日志和响应不得包含 Key、JWT、Cookie、prompt、完整上游 body 或资源 token。
10. Off/Shadow 的既有流式行为保持不变；只有 Enforce 且显式注册了流式 adapter 才进入新链路。

## 4. 状态机

### 4.1 请求状态

在 schema v7 中扩展 `requests.state` 的允许值：

```text
received → validating → reserved → queued → dispatched → completing
                                                        ├→ succeeded → settled
                                                        ├→ failed → settled
                                                        ├→ cancel_requested → canceled → settled
                                                        └→ unknown → settled
reserved/queued ─→ cancel_requested
cancel_requested ─→ unknown
```

`cancel_requested` 表示网关已经记录用户取消意图，不表示上游已经停止；`canceled` 只表示 adapter 明确确认取消。未知状态仍可由管理员/对账流程处理，不能由 lease TTL 自动 release。

### 4.2 Upstream lease

```text
held → active → succeeded | failed | unknown
held/active → unknown       (断流、超时、进程退出、取消未确认)
held/active → failed        (明确拒绝或确认取消)
```

lease 的 `reconcile_until_ms`、`upstream_request_ref` 和 allowlisted `error_kind` 继续作为对账依据。流开始后第一次有效事件前可仍为 `held`，适配器开始向客户端转发后必须 heartbeat 为 `active`。

## 5. 适配器契约

在 Tauri/Core bridge 层增加不携带凭据的流式请求和事件边界：

```rust
pub struct StreamUsage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

pub struct StreamEvent {
    pub data: serde_json::Value,
    pub usage: Option<StreamUsage>,
    pub upstream_request_ref: Option<String>,
}

pub enum CancelSupport {
    Confirmed,
    Unsupported,
    Unknown,
}

pub trait StreamSink {
    fn emit(&mut self, event: StreamEvent) -> bool;
    fn cancel_requested(&self) -> bool;
}

pub enum StreamTerminalOutcome {
    Success { actual_units: Option<i64>, upstream_request_ref: Option<String> },
    Rejected { status: u16, code: String, accepted: bool },
    Canceled { upstream_request_ref: Option<String> },
    TransportUnknown { reason: String, upstream_request_ref: Option<String> },
}

pub trait LeaseStreamAdapter: Send + Sync {
    fn execute_stream(
        &self,
        lease: &aiwork_core::UpstreamLeaseGrant,
        request: aiwork_core::ChatExecutionRequest,
        sink: &mut dyn StreamSink,
    ) -> StreamTerminalOutcome;

    fn cancel_stream(&self, _lease: &aiwork_core::UpstreamLeaseGrant) -> CancelSupport {
        CancelSupport::Unsupported
    }
}
```

`StreamUsage` 只是协议观测字段，不直接换算用户额度；除非已有明确 cost policy/adapter 证据，否则 `actual_units` 必须保持 `None`。`StreamSink::emit` 返回 `false` 时代表客户端通道已关闭；adapter 不得把它当作成功。`StreamSink::cancel_requested` 用于让同步 adapter 在读取循环中观察取消意图。`CancelSupport::Confirmed` 才能产生 `Canceled`，`Unsupported`/`Unknown` 只能产生 `TransportUnknown`。Mock adapter 必须覆盖成功、多事件、明确拒绝、客户端断开、取消确认、取消不支持和中途断流。

`CoreUpstreamExecutor` 为每个显式 `(account_ref, provider, credentials_ref)` 注册流式 adapter，并在 dispatch 前执行与非流式相同的 binding/credential 校验。只有 `Acquired` grant 可调用 `execute_stream`；Replay 不可执行。

## 6. Gateway 流程

1. 认证层建立 Core `Principal`，校验 `chat:invoke`，把协议投影后的模型和请求体交给同一个 Chat cost policy。
2. Core 以 idempotency scope + canonical body hash 执行原子 preflight，创建用户 reservation 和 upstream lease；Replay 返回持久化请求状态，不创建新流。
3. Gateway 启动流响应和 heartbeat 任务。heartbeat 使用同一 Principal、lease id 和配置 TTL，周期小于 lease TTL；heartbeat 失败立即停止继续假设成功，并由执行线程按未知结算。
4. Adapter 只收到 Core grant 和无凭据请求，向 `StreamSink` 发送标准化事件；协议层分别投影为 OpenAI SSE、Responses SSE 或 Anthropic SSE，并在终止事件处发送协议要求的结束标记。
5. 终止结果只经过一个 settlement helper：成功 commit，未接受拒绝 release，确认取消 release，未知保留 unknown；重复 settlement 不重复账本、health 或 audit。
6. 客户端断开后记录 `cancel_requested`，尝试调用 adapter cancel；没有明确确认时 lease/request 进入 unknown。Core 重启时恢复 active/unknown lease，不能自动重复发送流。

### 6.1 幂等重放

流式事件和完整响应正文不写入 Core。相同幂等键始终不触发第二次上游执行：

- 请求仍在 `reserved`/`queued`/`dispatched`/`completing`：返回稳定的 `409 request_in_progress` 和脱敏 request id；
- 请求已终态：返回稳定的终态 metadata 和原 error category；不伪造第二条 SSE 流；
- hash 不同：返回现有 `idempotency_conflict`。

## 7. 协议兼容矩阵

| 协议 | Enforce stream | 成功结束 | 明确拒绝 | 未知/断流 | 当前阶段 |
|---|---|---|---|---|---|
| OpenAI Chat | 支持 | `data` chunks + `[DONE]` | OpenAI error envelope | SSE error/断流后关闭，状态 unknown | 实现 |
| OpenAI Responses | 支持 | Responses event sequence + terminal event | Responses error envelope | 状态 unknown | 实现 |
| Anthropic Messages | 支持 | `message_start/content_block/.../message_stop` | Anthropic error envelope | 状态 unknown | 实现 |
| OpenAI Completions | 继续 501 | 不改变旧路径 | 不改变旧路径 | 不改变旧路径 | 后续评估 |
| Embeddings | 继续 501/现有语义 | 不进入本阶段 | 不进入本阶段 | 不进入本阶段 | 不变 |

Off/Shadow 继续调用现有 `stream_chat`/`wb_stream_chat`；新 contract tests 不调用真实网络。

## 8. Schema 与迁移

- schema v7 只扩展 `requests` 状态约束和必要的请求结果字段，不修改既有 quota/lease 语义。
- v6→v7 在一个 SQLite 事务中重建受约束的 `requests`/`idempotency_keys` 表，保留所有合法旧记录；旧记录状态保持不变。
- 迁移失败整体回滚，`core_mode` 不自动切换为 enforce；迁移报告只包含计数、哈希和 allowlisted 状态。
- 不把 SSE 正文、prompt、凭据、完整上游响应或客户端 Key 写入新表。

## 9. 测试与观测

### Core

- schema v6→v7 保留数据、幂等和旧状态；取消状态转换矩阵拒绝非法跳转；取消确认 release，取消未知保留 unknown。
- heartbeat 所有权、终态无副作用、TTL 恢复、重复 settlement 幂等。
- 同一幂等键并发请求只产生一个 lease；Replay 不可执行；不同 body 稳定冲突。

### Tauri/Mock

- 三种协议的事件顺序、结束标记、错误 envelope 和 usage 缺失行为。
- 客户端 channel 关闭、adapter 断流、cancel confirmed/unsupported/unknown 均进入预期终态。
- Core enforce 未注册 stream adapter 时返回 501 且无 reservation/lease；不会触发 legacy pool。
- 日志/审计无 prompt、Key、JWT、Cookie、完整 URL token 或原始 SSE body。

### 观测

沿用现有结构化 request/lease/audit 事件，增加 `stream_started`、`stream_first_event`、`stream_completed`、`cancel_requested`、`stream_unknown` 的 allowlisted 分类和耗时；指标只记录计数、延迟、协议、模型、脱敏 account id 和 request/lease id。

## 10. 发布与回滚

新流式 enforce 路径必须由显式配置/adapter registry 开启；默认行为不变。发布前只使用 D 盘隔离目录、`--offline --locked` 和 Mock/fixture；不运行会触碰真实上游的测试。发现问题时关闭 stream enforce feature gate，保留 Core 数据与 audit 现场，回滚到 off/shadow，不把新状态反写旧 JSON。

## 11. 完成判定

Phase 3A 只有在以下证据全部具备时才可标记完成：schema v7 迁移测试通过；Core 状态、quota/lease settlement、幂等和恢复测试通过；三种协议 Mock 流式 contract tests 通过；Tauri enforce 路由没有 legacy fallback；D 盘验证日志可复核；真实上游未联调部分明确保持 fail-closed。素材和视频不属于本阶段完成项。

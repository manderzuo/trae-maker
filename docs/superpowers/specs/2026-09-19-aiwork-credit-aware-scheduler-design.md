# AI Work Assistant Phase 2 积分感知调度与上游租约设计

## 1. 目标与阶段边界

本阶段把 Phase 0/1 的用户额度预占接到可恢复的多账号上游调度：Core 保存账号目录、积分/能力观测和 upstream lease；调度器在同一事务规则下选择健康账号并占用并发槽；Tauri 进程内的 Agent 适配器先执行已存在的 Trae/WB 请求路径；后续 Phase 4 再把 Agent 拆为只监听回环地址的独立进程。

本阶段的成功条件是：

1. Core enforce 模式下，已接入的非流式 Chat 请求在调用上游前同时拥有用户 quota reservation 和 upstream lease。
2. 账号选择只使用启用、能力/区域匹配、观测新鲜、冷却结束且并发槽未满的候选；没有可信候选时 fail closed，不退回旧池绕过预算。
3. lease、观测和错误分类在重启后可恢复；明确拒绝释放，超时/断连进入 unknown 并保留对账依据。
4. 用户永久额度与上游积分观测始终是不同表、不同语义、不同结算路径。
5. Mock executor/observer 覆盖全部新增闭环；测试不读取真实凭据、不访问真实生成接口、不把真实余额写成用户 grant。

本阶段不实现流式 lease 生命周期、素材用户归属迁移、视频 jobs/attempts、取消/重启对账 worker、独立 Core/Agent 进程、管理 UI 和公网部署。这些能力只能复用本阶段稳定的 lease/observation 接口，分别进入后续阶段。

## 2. 已核验的现状

- `src-tauri/src/api_server/pool.rs` 的 `ApiPool` 是进程内 `Mutex<HashMap>`；`pick_excluding` 只更新 `last_used`，没有持久化 lease、账号级并发槽或重启恢复。
- `src-tauri/src/commands/api_server.rs` 启动时读取 `api_pool.json`、`account_cooldowns.json`、`remaining_credits.json` 和账号凭据，并把它们同步到两个内存池。
- Trae 额度查询在 `src-tauri/src/commands/accounts.rs` 中解析 entitlement pack，得到 total/general/work 和最近过期时间；WorkBuddy 额度查询通过现有 Python/缓存路径完成。两者都是上游观测，不是用户 Core quota。
- 非流式 Chat 当前在 `routes.rs` 中完成 Core 用户 reservation 后仍直接进入旧的 `ApiPool` 选号和上游请求；全局 `inflight` 只表示进程级请求数，不能保证单账号并发上限。
- `src-core` 当前 schema v5 已有 `upstream_observations` 基础表，但没有 `upstream_accounts`、`upstream_leases` 或账号级持久化健康状态。
- 现有凭据由 Tauri vault/账号存储恢复为运行时 JWT/token；Core 数据库不得保存这些秘密。调度接口只传递不含秘密的 `account_ref` 和由适配器解析的 `credentials_ref`。

## 3. 方案选择

### 3.1 方案 A：只扩展现有 ApiPool

在 `ApiPool` 里增加内存计数、候选 freshness 和一个轻量文件锁。实现快，但重启会丢失 lease，多个进程无法共享并发槽，用户 quota 与账号选择也无法原子关联；它只能作为 legacy/off 兼容路径，不能成为 enforce 权威。

### 3.2 方案 B：Core 持久化调度权威，Tauri 内置 Agent 先行（采用）

在 `src-core` 做 schema v6 和事务型 `SchedulerStore`，增加 `upstream_accounts`、追加式 `upstream_observations` 和 `upstream_leases`。Tauri 通过明确的 `ObservationReader`/`UpstreamExecutor` port 适配现有 Trae/WB 实现；Mock 实现用于测试。`ApiPool` 继续服务 off 模式，并在 shadow 模式作为对照，不再在 enforce 中决定账号。

该方案满足重启、并发和 fail-closed 要求，同时不提前引入 Windows 子进程编排。Phase 4 可把相同 port 移到 Local Agent，而不改变 Core 数据模型和客户端 API。

### 3.3 方案 C：本阶段立即拆出独立 Local Agent

一次性加入进程密钥、Job Object、端口协商、凭据传递和桌面生命周期管理，架构完整但会把调度正确性与部署风险绑在一起，难以先证明 lease 状态机。它保留为 Phase 4，不作为本阶段实现路线。

## 4. 组件职责与权威边界

### 4.1 Core Scheduler

Core Scheduler 是 enforce 模式的唯一账号选择和 lease 写入者。它只读取：账号目录、观测、持久化冷却、请求约束和 cost policy；它不读取 JWT、Cookie 或上游原始响应，也不直接发 HTTP。

### 4.2 Tauri 内置 Agent Adapter

Phase 2 中 Agent 仍运行在 Tauri 进程内，但通过 port 接口接收 Core 签发的 lease。它负责：

- 以 opaque `credentials_ref` 从现有受保护凭据存储解析运行时凭据；
- 将 lease 的 provider/region/capability 约束映射到现有 Trae 或 WorkBuddy 请求构造；
- 把上游结果归一为 `Success`、`Rejected`、`TransportUnknown` 或 `CapabilityMismatch`；
- 只在明确的用户业务请求中发起生成调用；观测刷新使用独立的只读 reader。

Agent 不监听公网，不接受客户端传来的 account id 作为授权依据，不把凭据写入 Core、响应、日志或 audit metadata。

### 4.3 现有 ApiPool

- `off`：保持当前行为，兼容旧 JSON、旧策略、现有真实路由。
- `shadow`：可作为候选对照和 parity 诊断，但不写 user quota，不创建会影响请求放行的 lease。
- `enforce`：不再调用 `pick_excluding*` 决定业务账号；任何未接入 Scheduler 的 endpoint 必须返回明确的 `scheduler_endpoint_not_enabled`/501 或等价 fail-closed 错误，不能静默回退 legacy。

## 5. Schema v6 与数据模型

所有 Core 时间使用 UTC 整数毫秒；用户 quota 和 lease 预算使用整数逻辑单位。上游积分的原始小数不直接变成用户额度，观测记录必须带明确的 `value_scale` 和 source 语义。

### 5.1 `upstream_accounts`

```text
id                  TEXT PRIMARY KEY       -- Core 内稳定 opaque account_ref
provider            TEXT NOT NULL          -- trae | workbuddy | mock
credentials_ref     TEXT NOT NULL          -- 只引用受保护凭据，不是秘密本身
region              TEXT                   -- 适配器声明的区域
capabilities_json   TEXT NOT NULL          -- 已确认能力集合，不猜测未知能力
enabled             INTEGER NOT NULL       -- 0/1
max_concurrency     INTEGER NOT NULL       -- > 0；0 不表示无限
state               TEXT NOT NULL          -- available | cooling | forbidden | disabled
cooldown_until_ms   INTEGER
cooldown_reason     TEXT
consecutive_errors  INTEGER NOT NULL
created_at_ms       INTEGER NOT NULL
updated_at_ms       INTEGER NOT NULL
```

`id` 可由 provider 与稳定账号标识派生，但不得在普通用户响应中返回。`credentials_ref` 不允许包含 JWT、Cookie、refresh token 或可逆密文。

### 5.2 `upstream_observations`

保留现有表并补足唯一索引/版本语义，不删除 Phase 0/1 的 `source=json_cache` 迁移记录：

```text
id                  TEXT PRIMARY KEY
account_ref         TEXT NOT NULL REFERENCES upstream_accounts(id)
resource_kind       TEXT NOT NULL
observed_value      INTEGER             -- 按 value_scale 归一；无法证明时为 NULL
value_scale         INTEGER NOT NULL    -- 例如 100 表示保留两位小数
source              TEXT NOT NULL       -- adapter-owned opaque source label
status              TEXT NOT NULL       -- fresh | stale | failed
observed_at_ms      INTEGER NOT NULL
stale_at_ms         INTEGER NOT NULL
summary_json        TEXT NOT NULL       -- 脱敏摘要；不得含凭据或完整响应
```

一条新观测追加新记录；不覆盖历史，不把失败写成余额为零。选择器只接受 `status=fresh`、当前时间早于 `stale_at_ms` 且 source 被当前 policy 允许的记录。旧 `json_cache` 观测默认只用于 shadow/诊断；enforce 必须使用 Phase 2 reader 明确产生的 fresh 观测。

### 5.3 `upstream_leases`

```text
id                    TEXT PRIMARY KEY
request_id            TEXT NOT NULL REFERENCES requests(id)
account_ref           TEXT NOT NULL REFERENCES upstream_accounts(id)
resource_kind         TEXT NOT NULL
predicted_units       INTEGER NOT NULL CHECK(predicted_units > 0)
observation_id        TEXT REFERENCES upstream_observations(id)
state                 TEXT NOT NULL CHECK(state IN
                        ('held','active','succeeded','failed','unknown','released'))
lease_expires_at_ms   INTEGER NOT NULL
reconcile_until_ms    INTEGER
upstream_request_ref  TEXT
error_kind            TEXT
created_at_ms         INTEGER NOT NULL
updated_at_ms         INTEGER NOT NULL
settled_at_ms         INTEGER
UNIQUE(request_id, resource_kind)
```

数据库索引按 `(account_ref, state)`、`(request_id, resource_kind)` 和 `state='held'/'active'/'unknown'` 的恢复查询建立。active/held lease 的数量是账号并发槽判断的唯一依据；不能用 `ApiSharedState.inflight` 代替。

### 5.4 迁移规则

v5→v6 在单个 SQLite 事务内建立新表、索引和检查约束。现有启用账号可同步为 `upstream_accounts`，但只写 opaque credential reference；`remaining_credits.json` 和 WorkBuddy cache 只生成 `source=json_cache` 的历史/待核对 observation，不生成用户 grant，也不自动变成 enforce 可用观测。迁移失败回滚整个 schema 变更，`scheduler_mode` 保持 `off`。

## 6. 内部接口契约

### 6.1 ObservationReader

```rust
pub struct ObservationRequest {
    pub account_ref: String,
    pub provider: String,
    pub resource_kind: String,
}

pub struct ObservationSnapshot {
    pub account_ref: String,
    pub resource_kind: String,
    pub available_units: Option<i64>,
    pub value_scale: i64,
    pub source: String,
    pub observed_at_ms: i64,
    pub stale_at_ms: i64,
    pub capabilities: Vec<String>,
    pub region: Option<String>,
    pub summary: serde_json::Value,
}

pub trait ObservationReader: Send + Sync {
    fn read(&self, request: ObservationRequest) -> Result<ObservationSnapshot, ObservationError>;
}
```

Trae reader复用现有 entitlement pack 解析，WorkBuddy reader复用现有只读 credits 解析；二者都必须把 source、时间和 scale 写清楚。网络失败返回错误并使选择器拒绝过期观测，不把旧值改成零。`MockObservationReader` 固定返回 fixture，不访问 `AGENT_HOST`、真实 billing URL 或本机凭据。

### 6.2 Scheduler

```rust
pub struct LeaseRequest {
    pub request_id: String,
    pub user_id: String,
    pub resource_kind: String,
    pub provider_hint: Option<String>,
    pub required_capabilities: Vec<String>,
    pub region: Option<String>,
    pub predicted_units: i64,
    pub safety_margin_units: i64,
    pub observation_max_age_ms: i64,
    pub allowed_accounts: Option<Vec<String>>,
    pub dedicated_account: Option<String>,
}

pub struct UpstreamLeaseGrant {
    pub lease_id: String,
    pub account_ref: String,
    pub credentials_ref: String,
    pub observation_id: String,
    pub predicted_units: i64,
    pub lease_expires_at_ms: i64,
}

pub trait Scheduler: Send + Sync {
    fn acquire(&self, request: LeaseRequest) -> Result<UpstreamLeaseGrant, ScheduleError>;
    fn heartbeat(&self, lease_id: &str) -> Result<(), ScheduleError>;
    fn settle(&self, lease_id: &str, outcome: LeaseOutcome) -> Result<(), ScheduleError>;
}
```

`acquire` 使用 `BEGIN IMMEDIATE`，在同一事务中完成：请求幂等检查、候选筛选、fresh observation 检查、active/held slot 计数、lease 插入和审计 metadata。若没有候选，返回可诊断的 `no_fresh_observation`、`no_capacity`、`capability_mismatch` 或 `account_cooling`，不写半个 lease。

### 6.3 UpstreamExecutor

```rust
pub struct UpstreamChatRequest {
    pub lease_id: String,
    pub model: String,
    pub body: serde_json::Value,
}

pub enum UpstreamOutcome {
    Success { body: serde_json::Value, actual_units: Option<i64>, upstream_request_ref: Option<String> },
    Rejected { status: u16, code: Option<String>, accepted: bool },
    TransportUnknown { reason: String, upstream_request_ref: Option<String> },
}

pub trait UpstreamExecutor: Send + Sync {
    fn execute_chat(&self, request: UpstreamChatRequest) -> Result<UpstreamOutcome, ExecutorError>;
}
```

`accepted=false` 的明确拒绝释放用户 reservation 和 lease；`accepted=true` 或无法确认是否接受都进入 unknown。真实适配器只从 lease 获取账号引用，不能接受请求体中的 `user_id`/`account_ref` 覆盖 Core 决定。

## 7. 调度与结算状态机

### 7.1 主请求顺序

```text
authenticate/scope
  -> validate + CostPolicy estimate
  -> Core atomic request + user reservation + upstream lease
  -> Agent execute with lease
  -> Core atomic settle user quota + lease + request + audit
  -> protocol response
```

lease 获取失败时不创建用户 reservation；如果实现采用先后两个事务，必须使用已有 reservation id 做补偿，并在进程重启恢复时将孤儿 reservation 标记为可审计的失败，而不能悄悄继续调用上游。本阶段推荐直接扩展 Core 的 preflight，使两个写入在同一 `BEGIN IMMEDIATE` 中完成。

### 7.2 结算矩阵

| Agent 结果 | 用户 quota | upstream lease | 账号状态 |
|---|---|---|---|
| 明确成功且实际用量可信 | `commit(actual)`，释放多余预占 | `succeeded` | success，重置连续错误 |
| 明确成功但实际用量未知 | 按预占保守 commit，标记 `actual_unknown` | `succeeded` | success，但记录 unknown usage |
| 明确拒绝且确认未接受 | release | `failed` | 按错误分类冷却/禁用 |
| timeout、断连、进程异常 | 保留为 `unknown` | `unknown` 至 `reconcile_until_ms` | transport/server 冷却；不立即复用该 lease |
| lease 过期而无结果 | 不自动 release | `unknown` | 启动恢复/对账队列可见 |

重复 settle 必须返回首次最终结果，不得重复扣减、释放槽位或覆盖更早的 unknown。unknown 不是失败的别名，不能由 TTL 自动释放。

### 7.3 账号状态

保留现有错误分类语义并持久化到 `upstream_accounts`：

- `HardCredit`：账号明确无可用上游积分，进入长冷却，直到只读刷新证明恢复；
- `PlanLimit`/`SoftRate`/`NotFound`：短/中冷却，保留原因；
- `Server`/timeout：指数退避并增加连续错误；
- `SessionDead`/`Forbidden`：禁用，必须重新验证凭据或管理员启用；
- reader 失败：不把账号判为零额度，保留最后成功观测但标记 stale，enforce 下不可选。

## 8. 运行模式与兼容矩阵

新增 `scheduler_mode=off|shadow|enforce`，默认 `off`，与现有 `core_mode` 组合如下：

| core_mode | scheduler_mode | 行为 |
|---|---|---|
| off | 任意 | 完全保留旧 JSON/API pool 路径；不写 Core lease |
| shadow | off | 现有 legacy 路径；不运行 Scheduler |
| shadow | shadow | legacy 仍放行；Scheduler 只读评估、记录候选/失配/stale，不扣用户额度、不阻断请求 |
| enforce | off | 已接入 Core 预算但没有调度器，预算型 endpoint 返回 `scheduler_not_ready`，不回退旧池 |
| enforce | shadow | 配置错误，启动/请求 fail closed，不能用 shadow 结果放行 |
| enforce | enforce | 已接入 endpoint 使用 Core atomic reservation + upstream lease；未接入 endpoint 返回 501/稳定错误 |

Phase 2 首先接入 `/v1/chat/completions` 的非流式 Core 路径；Phase 1 已明确为 501 的流式请求保持 501。Responses、Anthropic、Completions、图片和视频在尚未有对应 lease adapter 前不得在 enforce 中绕过 Scheduler；off/shadow 继续保留现有兼容行为。

## 9. 安全、隔离与观测

- 普通用户只看到自己的 request/quota 状态和稳定错误码；account_ref、观测来源、lease 选择原因只进入管理员审计和脱敏诊断。
- key 的 allowed/dedicated account 约束在 Core Scheduler 过滤；请求体字段不能覆盖；Core 从认证 Principal 取得 user/key，不接受客户端 actor/user_id。
- Core SQLite 不存 JWT、Cookie、refresh token、完整上游响应、prompt 或完整输出；`summary_json` 经过字段白名单和长度上限处理。
- 审计 metadata 至少含 request_id、lease_id、脱敏 account_ref、provider、resource_kind、observation_id、selection_reason、错误分类和时间；不含凭据。
- 指标至少包括 fresh/stale observation、lease acquire success/reject、active/unknown lease、per-account slot saturation、cooldown、unknown settle、scheduler fallback attempt（应为零）和 reader failures。
- `/health`/`/healthz` 仍只返回存活状态；账号、观测和 lease 详情不进入公开健康响应。

## 10. 测试与验收

### Core 测试

1. v5→v6 可重复迁移、外键/WAL/检查约束和失败回滚。
2. 两个并发 acquire 竞争一个 `max_concurrency=1` 账号时恰好一个成功；不同账号可并行；重启后 active/held lease 可被扫描。
3. stale、failed、`json_cache` 禁止用于 enforce；fresh reader 观测可用；value scale 不同不能直接比较。
4. provider/region/capability、allowed/dedicated、cooldown 和 HardCredit 过滤正确；无候选给出稳定错误且不写用户 reservation。
5. 同一 request 重试只产生一个 lease；hash 冲突返回幂等冲突；重复 settle/heartbeat 无副作用。
6. success/rejected/unknown/expired 的 quota 与 lease 结算符合矩阵；unknown 不因 TTL 自动释放。
7. audit 和 summary 中没有 JWT、Cookie、Key、prompt 或完整上游响应。

### Tauri/适配器测试

1. Mock Trae/WB observation reader 只读 fixture；Mock executor 记录调用次数并验证 lease id/account ref 来自 Core，而非请求体。
2. 非流式 Chat 在 enforce 下调用 Mock scheduler/executor；预算不足、stale、并发满和 timeout 都不访问真实 `AGENT_HOST`/WorkBuddy billing/当前运行服务。
3. legacy/off/shadow 的既有路由、池策略和响应 contract 不回归；enforce 下未接入 endpoint 不会静默走旧池。
4. 现有 Rust、前端和 Python 单测继续通过；不把真实 upstream 测试作为 Phase 2 证据。

## 11. 迁移、发布与回滚

1. 发布前停机备份 Core SQLite、旧 JSON、凭据 vault 和现有数据目录；先以 `scheduler_mode=off` 启动并检查 schema/parity。
2. 运行只读 observation refresh，比较 provider/account/resource/scale/新鲜度；任何无法确认的账号保持 stale/disabled，不猜测能力或余额。
3. 先 shadow 观察选择差异和 stale 比例，再仅对已接入非流式 Chat 的测试 Key 开启 enforce。
4. 任一 lease/observation 迁移异常都可将 `scheduler_mode` 退回 off；旧 ApiPool 路径仍可运行，Core 用户 quota 数据不删除、不反向导入上游余额。
5. 只有 Phase 2 的测试和 parity report 通过后，才允许把 enforce 作为显式管理员配置；默认值不改变。

## 12. 与后续阶段的接口承诺

- Phase 3 可复用 `Scheduler::acquire/heartbeat/settle` 处理流式、图片和视频，只需新增 resource kind、job/request 关联和 adapter capability。
- Phase 3 的视频恢复/取消必须把 job_attempt 的上游 request ref 关联到同一 lease，不得另造旁路计数。
- Phase 4 的独立 Local Agent 只替换 `ObservationReader`、`UpstreamExecutor` 的传输实现；Core schema、scope、quota 和状态机不变。
- 管理 UI、指标导出、Nginx/FRP 和备份恢复只能调用 Core 管理 API，不能直接编辑 SQLite 或继续扩展 JSON 为第二权威源。

# AI Work Assistant Core Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不消耗真实上游积分的前提下，建立可进程化的 `aiwork-core` 基础库，并把用户身份、API Key scope、SQLite 额度账本、请求幂等、Mock 上游和 Chat/Models 的安全闭环接入现有网关。

**Architecture:** 新增独立 `src-core` Rust crate，先以受控 path dependency 供现有 Tauri 网关使用；CoreStore 是 Phase 1 的唯一额度/身份写入源，未来可不改领域接口而提取为独立本地 Core 进程。现有 JSON、上游账号池和真实请求保持兼容模式，只有明确开启 Core enforce 后才执行新账本。

**Tech Stack:** Rust 1.77、Rust 2021、SQLite via `rusqlite` 0.32 bundled、Serde/JSON、Axum 0.7、Tokio 1、现有 Tauri 2、Sha-256 + constant-time comparison、Mock upstream。

**Spec:** `docs/superpowers/specs/2026-09-19-aiwork-unified-gateway-design.md`

## Global Constraints

- 仅支持当前项目的 Windows 运行约束，保持 Rust `rust-version = "1.77"`。
- 不升级版本号；不提交 `src-tauri/target*`、`data/`、凭据、JWT、真实 API Key 或日志中的秘密。
- 保留当前工作树已有修改；实现时只提交本计划产生的明确文件，不能 reset、checkout 或覆盖用户变更。
- `AIWORK_DATA_DIR` 继续决定数据根目录；Core 数据固定在 `<data_dir>/data/core.sqlite3`。
- API Key 绑定 `user_id`；服务端不信任请求体、查询参数或普通请求头中的 `user_id`。
- SQLite 是 Core 模式下的唯一权威写入源；旧 JSON 只读迁移输入，不能与 SQLite 双写账本。
- 所有额度/计数使用整数逻辑单位；没有 `CostPolicy` 的能力必须拒绝，不猜测上游单价或余额。
- 测试默认使用 Mock upstream；不得为验证本计划发起真实生成、真实视频、真实图片或消耗型积分请求。
- `core_mode=off` 保持现有行为；`shadow` 只记录可观测信息不预占；`enforce` 才启用新身份、额度和幂等闭环。
- 每个任务先写失败测试，再写最小实现，再运行针对性测试，最后提交独立可审查的 commit。

## 子项目边界

本计划只覆盖设计稿的 Phase 0 和 Phase 1，形成一个可以独立测试的安全最小闭环：

1. CoreStore、Schema 和迁移版本。
2. 用户、Key、scope、Core 鉴权与安全 Key 发行。
3. 用户额度 grant/reserve/commit/release/adjust 和并发不超额。
4. 请求记录、规范化 hash 和 Idempotency-Key 冲突检测。
5. 可版本化的逻辑成本策略和 Mock upstream 端口。
6. 现有网关的 Core `models`/非流式 Chat 接入，以及旧模式兼容开关。
7. 旧 JSON 检查/映射报告与 Tauri 管理命令骨架。

流式、Responses/Anthropic 的新账本接入、真实积分 observation、账号 lease、视频、素材、取消、重启对账、独立 Core/Agent 进程打包和公网部署属于后续独立计划；本计划为这些功能提供稳定的 store/port 接口。

## 文件地图

### 新建

- `src-core/Cargo.toml`：可复用 Core crate 的依赖和测试配置。
- `src-core/src/lib.rs`：公开模块与稳定导出。
- `src-core/src/error.rs`：Core 错误枚举和 SQLite/输入错误转换。
- `src-core/src/schema.rs`：Schema 版本、迁移 SQL、表约束。
- `src-core/src/models.rs`：User、Principal、Reservation、Request 等领域 DTO。
- `src-core/src/store.rs`：带 `BEGIN IMMEDIATE` 的 SQLite Store 基础层。
- `src-core/src/identity.rs`：高熵 API Key 发行、hash 校验、scope 和用户操作。
- `src-core/src/quota.rs`：额度账本、并发预占和结算。
- `src-core/src/requests.rs`：规范化请求 hash、请求状态和幂等事务。
- `src-core/src/cost.rs`：版本化逻辑成本策略和保守估算。
- `src-core/src/ports.rs`：Core 与上游/网关之间的无网络领域接口。
- `src-core/tests/schema_bootstrap.rs`、`identity.rs`、`quota.rs`、`requests.rs`、`core_flow.rs`：Core crate 集成测试。
- `src-tauri/src/api_server/core_bridge.rs`：Tauri 网关到 CoreStore 的适配层。
- `src-tauri/src/api_server/core_migration.rs`：旧 JSON 只读检查、映射和迁移报告。
- `src-tauri/src/commands/core.rs`：本地管理员的 Core 状态、用户、Key、额度和迁移命令。

### 修改

- `src-tauri/Cargo.toml`：添加 `aiwork-core` path dependency；不改变现有 crate 的版本号。
- `src-tauri/src/api_server/mod.rs`：加入 Core 运行态字段、Core mode 和 bridge 导出。
- `src-tauri/src/api_server/gateway_settings.rs`：增加向后兼容的 `core_mode` 设置，默认 `off`。
- `src-tauri/src/api_server/auth.rs`：`enforce` 模式使用 Core Principal；旧模式保留现有 JSON Key 路径。
- `src-tauri/src/api_server/routes.rs`：Models scope、非流式 Chat 的预占/结算/幂等接入；流式在 Phase 1 明确返回未启用错误。
- `src-tauri/src/api_server/server.rs`：保持既有路由，只让共享 state 携带 Core bridge 所需状态。
- `src-tauri/src/commands/api_server.rs`：启动时按 mode 打开 CoreStore，启动前执行 schema migration，关闭时不删除 SQLite。
- `src-tauri/src/commands/mod.rs`、`src-tauri/src/main.rs`：注册 Core 管理命令。
- `docs/server-deployment.md`、`docs/user-manual.md`：补充 Core mode、SQLite 备份和 Mock 验证说明，不写入任何真实凭据。

---

### Task 1: 建立 `src-core` crate 与 SQLite Schema

**Files:**
- Create: `src-core/Cargo.toml`
- Create: `src-core/src/lib.rs`
- Create: `src-core/src/error.rs`
- Create: `src-core/src/schema.rs`
- Create: `src-core/src/models.rs`
- Create: `src-core/src/store.rs`
- Modify: `src-tauri/Cargo.toml`
- Test: `src-core/tests/schema_bootstrap.rs`

**Interfaces:**
- Produces `pub struct CoreStore`，`pub fn CoreStore::open(data_dir: &Path) -> Result<Self, CoreError>`，`pub fn CoreStore::migrate(&self) -> Result<(), CoreError>`，`pub fn CoreStore::schema_version(&self) -> Result<u32, CoreError>`。
- Produces `pub const CORE_DB_FILE: &str = "core.sqlite3"` 和 `pub const CURRENT_SCHEMA_VERSION: u32 = 1`。
- Produces `pub type SharedCoreStore = Arc<CoreStore>`；`CoreStore` 内部使用 `Mutex<rusqlite::Connection>`，公开方法不可把 `Connection` 泄露给网关。

- [ ] **Step 1: 写 Schema 失败测试**

在 `src-core/tests/schema_bootstrap.rs` 创建随机临时数据目录，调用 `CoreStore::open` 和 `migrate`，断言 `data/core.sqlite3` 存在、版本为 1、外键开启，并能查询以下表：`schema_meta`、`users`、`api_keys`、`cost_policies`、`quota_ledger`、`quota_reservations`、`requests`、`idempotency_keys`、`upstream_observations`、`audit_events`。

```rust
#[test]
fn bootstrap_creates_authoritative_schema() {
    let dir = test_dir("schema");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
    assert!(dir.join("data/core.sqlite3").is_file());
    assert_eq!(store.table_count("users").unwrap(), 1);
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test schema_bootstrap bootstrap_creates_authoritative_schema -- --nocapture`

预期：因 crate、`CoreStore` 或表不存在而失败。

- [ ] **Step 3: 创建 crate 和最小 Schema 实现**

`src-core/Cargo.toml` 使用 Rust 2021，并声明 `serde`、`serde_json`、`chrono`、`rand`、`base64`、`sha2`、`subtle`、`rusqlite = { version = "0.32", features = ["bundled"] }`、`thiserror`。Schema 第一版至少包含以下约束：

```sql
CREATE TABLE schema_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('admin','operator','user')),
  status TEXT NOT NULL CHECK(status IN ('active','disabled')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE api_keys (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  key_digest BLOB NOT NULL UNIQUE,
  scopes_json TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('active','revoked')),
  created_at_ms INTEGER NOT NULL,
  revoked_at_ms INTEGER
);
CREATE TABLE cost_policies (
  id TEXT PRIMARY KEY,
  endpoint TEXT NOT NULL,
  model_pattern TEXT NOT NULL,
  resource_kind TEXT NOT NULL,
  reserve_amount INTEGER NOT NULL CHECK(reserve_amount > 0),
  max_actual_amount INTEGER,
  version INTEGER NOT NULL,
  enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
  UNIQUE(endpoint, model_pattern, resource_kind, version)
);
CREATE TABLE quota_ledger (
  entry_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  resource_kind TEXT NOT NULL,
  event_kind TEXT NOT NULL,
  amount INTEGER NOT NULL CHECK(amount >= 0),
  delta INTEGER NOT NULL,
  request_id TEXT,
  actor_user_id TEXT,
  reason TEXT,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE quota_reservations (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  request_id TEXT NOT NULL UNIQUE,
  resource_kind TEXT NOT NULL,
  amount INTEGER NOT NULL CHECK(amount > 0),
  state TEXT NOT NULL CHECK(state IN ('held','committed','released','unknown')),
  expires_at_ms INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  settled_at_ms INTEGER
);
CREATE TABLE requests (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  api_key_id TEXT NOT NULL REFERENCES api_keys(id),
  protocol TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  state TEXT NOT NULL,
  result_status INTEGER,
  error_code TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE idempotency_keys (
  scope TEXT NOT NULL,
  client_key TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  request_id TEXT NOT NULL REFERENCES requests(id),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(scope, client_key)
);
CREATE TABLE upstream_observations (
  id TEXT PRIMARY KEY,
  account_ref TEXT NOT NULL,
  resource_kind TEXT NOT NULL,
  observed_value INTEGER,
  source TEXT NOT NULL,
  observed_at_ms INTEGER NOT NULL,
  stale_at_ms INTEGER,
  summary_json TEXT NOT NULL
);
CREATE TABLE audit_events (
  id TEXT PRIMARY KEY,
  actor_user_id TEXT,
  action TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id TEXT,
  request_id TEXT,
  metadata_json TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);
```

`CoreStore::open` 创建 `data/` 目录、设置 WAL/foreign_keys/busy_timeout，并在 `migrate` 中用事务执行迁移。错误通过 `CoreError` 保留 SQLite 原因和稳定分类。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test schema_bootstrap -- --nocapture`

预期：PASS；再运行 `cargo check --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix`，确认 path dependency 能解析。

- [ ] **Step 5: 提交**

```powershell
git add src-core src-tauri/Cargo.toml
git commit -m "feat: add core sqlite foundation"
```

### Task 2: 实现用户、API Key 和 scope

**Files:**
- Create: `src-core/src/identity.rs`
- Modify: `src-core/src/lib.rs`, `src-core/src/models.rs`, `src-core/src/store.rs`
- Test: `src-core/tests/identity.rs`

**Interfaces:**
- `pub struct Principal { pub user_id: String, pub key_id: String, pub scopes: BTreeSet<String> }`。
- `pub struct NewUser { pub id: String, pub name: String, pub role: UserRole }`。
- `pub struct IssuedApiKey { pub id: String, pub plaintext: String, pub prefix: String, pub user_id: String, pub scopes: BTreeSet<String> }`；plaintext 只从发行函数返回一次，不写入任何日志/表。
- `pub fn create_user(&self, input: NewUser, actor: &str) -> Result<User, CoreError>`。
- `pub fn issue_api_key(&self, user_id: &str, name: &str, scopes: BTreeSet<String>, actor: &str) -> Result<IssuedApiKey, CoreError>`。
- `pub fn authenticate_api_key(&self, presented: &str) -> Result<Principal, AuthError>`。
- `pub fn require_scope(principal: &Principal, scope: &str) -> Result<(), AuthError>`。

- [ ] **Step 1: 写身份失败测试**

覆盖：创建 admin/user、Key 只展示一次、正确 Key 命中、错误/撤销 Key 拒绝、scope 缺失拒绝、Key 不能访问其他用户资源。测试断言数据库中只出现 digest/prefix，不能出现完整 plaintext。

```rust
#[test]
fn issued_key_is_hash_only_and_authenticates_its_user() {
    let store = test_store();
    let user = store.create_user(user("u1", UserRole::User), "bootstrap").unwrap();
    let issued = store.issue_api_key(&user.id, "test", scopes(&["models:read"]), "bootstrap").unwrap();
    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();
    assert_eq!(principal.user_id, "u1");
    assert!(require_scope(&principal, "models:read").is_ok());
    assert!(store.raw_text_search("api_keys", &issued.plaintext).unwrap().is_empty());
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test identity -- --nocapture`

预期：因 identity API 和表操作不存在而失败。

- [ ] **Step 3: 实现高熵发行和校验**

使用 `OsRng` 生成 32 字节随机材料，格式为 `aw_live_` 加 base64url 无 padding 文本。使用 SHA-256 生成 `key_digest`，用 `subtle::ConstantTimeEq` 比较 digest；数据库只保存 digest、prefix、user id、scope、状态和时间。scope 序列化必须排序，避免同一 Key 因集合顺序产生不同数据。

用户和 Key 的写入放在同一事务；撤销使用条件更新 `status='active'`，重复撤销返回幂等成功。所有身份操作写 `audit_events`，metadata 只包含 id、scope 和结果，不包含 Key。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test identity -- --nocapture`

预期：所有身份和秘密不落盘测试 PASS。

- [ ] **Step 5: 提交**

```powershell
git add src-core
git commit -m "feat: add core user and api key identity"
```

### Task 3: 实现额度账本和并发预占

**Files:**
- Create: `src-core/src/quota.rs`
- Modify: `src-core/src/lib.rs`, `src-core/src/models.rs`, `src-core/src/store.rs`
- Test: `src-core/tests/quota.rs`

**Interfaces:**
- `pub struct QuotaGrant { pub user_id: String, pub resource_kind: String, pub amount: i64, pub actor_user_id: String, pub reason: String }`。
- `pub struct QuotaReserve { pub user_id: String, pub request_id: String, pub resource_kind: String, pub amount: i64, pub ttl_ms: i64 }`。
- `pub enum ReserveResult { Created(Reservation), Existing(Reservation), Insufficient { available: i64 } }`。
- `pub enum Settlement { Commit { actual_amount: Option<i64> }, Release, Unknown }`。
- `pub fn grant(&self, input: QuotaGrant) -> Result<QuotaBalance, CoreError>`。
- `pub fn reserve(&self, input: QuotaReserve) -> Result<ReserveResult, CoreError>`。
- `pub fn settle(&self, reservation_id: &str, settlement: Settlement) -> Result<QuotaBalance, CoreError>`。
- `pub fn balance(&self, user_id: &str, resource_kind: &str) -> Result<QuotaBalance, CoreError>`。

- [ ] **Step 1: 写额度失败测试**

测试 grant 100 后并发 32 个请求各 reserve 10，最多 10 个成功；第 11 个返回 `Insufficient`；重复同一 request id 返回原 reservation；重复 settle 不增加/释放第二次；unknown 保持保留状态；管理员 adjust 记录 actor/reason。

```rust
#[test]
fn concurrent_reservations_never_overdraw() {
    let store = Arc::new(test_store_with_grant("u1", "chat_request", 100));
    let barrier = Arc::new(Barrier::new(32));
    let results = run_threads(32, || {
        barrier.wait();
        store.reserve(QuotaReserve::new("u1", unique_request_id(), "chat_request", 10, 60_000))
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 10);
    assert_eq!(store.balance("u1", "chat_request").unwrap().available, 0);
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test quota -- --nocapture`

预期：因账本、事务和 reservation 状态不存在而失败。

- [ ] **Step 3: 实现事务性 reserve/settle**

所有 reserve 使用单个 SQLite `BEGIN IMMEDIATE`：读取 grant/adjust 的可用值，减去 `held` reservation，若不足则回滚；足够时同时写 `quota_reservations` 和 `quota_ledger(event_kind='reserve', delta=-amount)`。`request_id` 唯一约束负责同请求幂等。

`Release` 写正向差额；`Commit` 写 `event_kind='commit'` 的审计事件并只释放预占与实际用量之间的差额；`actual_amount=None` 按保守预占全部结算并标记 `actual_unknown`。只有 `held` 状态允许结算，其他状态返回原最终结果，不得二次改变余额。TTL 到期不能自动释放 unknown reservation。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test quota -- --nocapture`

预期：并发、幂等、unknown 和审计测试 PASS。

- [ ] **Step 5: 提交**

```powershell
git add src-core
git commit -m "feat: add transactional quota reservations"
```

### Task 4: 实现 CostPolicy、请求状态和幂等

**Files:**
- Create: `src-core/src/cost.rs`
- Create: `src-core/src/requests.rs`
- Modify: `src-core/src/lib.rs`, `src-core/src/models.rs`, `src-core/src/store.rs`
- Test: `src-core/tests/requests.rs`

**Interfaces:**
- `pub struct CostPolicy { pub id: String, pub endpoint: String, pub model_pattern: String, pub resource_kind: String, pub reserve_amount: i64, pub max_actual_amount: Option<i64>, pub version: i64, pub enabled: bool }`。
- `pub fn estimate(&self, endpoint: &str, model: &str, request: &Value) -> Result<CostEstimate, CostError>`；找不到策略返回 `BudgetPolicyMissing`，不返回默认费用。
- `pub enum BeginRequest { Created(RequestHandle), Existing(RequestHandle), Conflict }`。
- `pub fn begin_request(&self, input: BeginRequestInput) -> Result<BeginRequest, CoreError>`。
- `pub fn transition_request(&self, request_id: &str, expected: RequestState, next: RequestState, result: Option<RequestResult>) -> Result<(), CoreError>`。
- `pub fn canonical_json_hash(value: &serde_json::Value) -> [u8; 32]`。

- [ ] **Step 1: 写幂等和预算失败测试**

覆盖 JSON 字段顺序相同 hash、不同值不同 hash；同一 `(user_id, endpoint, Idempotency-Key)` 同 hash 返回 `Existing`；不同 hash 返回 `Conflict`；无启用策略返回 `BudgetPolicyMissing`；状态回退和重复 transition 被拒绝。

```rust
#[test]
fn idempotency_is_user_and_endpoint_scoped() {
    let store = test_store_with_user_and_key("u1");
    let a = json!({"model":"mock-1","messages":[{"role":"user","content":"hi"}]});
    let b = json!({"messages":[{"content":"hi","role":"user"}],"model":"mock-1"});
    let first = store.begin_request(input("u1", "k1", "/v1/chat/completions", "same", &a)).unwrap();
    let second = store.begin_request(input("u1", "k1", "/v1/chat/completions", "same", &b)).unwrap();
    assert!(matches!(first, BeginRequest::Created(_)));
    assert!(matches!(second, BeginRequest::Existing(_)));
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test requests -- --nocapture`

预期：因规范化 hash、request 表和状态条件更新不存在而失败。

- [ ] **Step 3: 实现规范化 hash、CostPolicy 和状态条件更新**

规范化 JSON 时递归按 object key 排序，保留数组顺序和 JSON 类型；hash 输入必须包含 endpoint、model 和 body，不能把用户身份放进可被客户端控制的 body。幂等 scope 使用 `format!("{}:{}", user_id, endpoint)`，数据库主键为 `(scope, client_key)`。

请求状态使用 `received/validating/reserved/queued/dispatched/completing/succeeded/failed/unknown/settled`；`transition_request` 使用 `UPDATE ... WHERE state=?`，更新数为 0 时返回 `InvalidTransition`。同步响应 Phase 1 只保存结果状态/错误摘要，不把完整 prompt/output 写入 SQLite。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test requests -- --nocapture`

预期：PASS；同时运行 `cargo test --manifest-path src-core/Cargo.toml` 确认前面三组测试不回归。

- [ ] **Step 5: 提交**

```powershell
git add src-core
git commit -m "feat: add core request idempotency and cost policy"
```

### Task 5: 建立网关 Core bridge、运行模式和 Mock upstream 端口

**Files:**
- Create: `src-tauri/src/api_server/core_bridge.rs`
- Create: `src-core/src/ports.rs`
- Modify: `src-core/src/lib.rs`, `src-tauri/Cargo.toml`, `src-tauri/src/api_server/mod.rs`
- Test: `src-core/tests/core_flow.rs`

**Interfaces:**
- `pub enum CoreMode { Off, Shadow, Enforce }`，实现 `TryFrom<&str>`，未知值返回配置错误而不是静默选择 enforce。
- `pub struct CoreBridge { pub store: Arc<CoreStore>, pub mode: CoreMode }`。
- `pub trait ChatExecutor: Send + Sync { fn execute(&self, request: ChatExecutionRequest) -> Result<ChatExecutionResult, UpstreamError>; }`。
- `pub struct MockChatExecutor { pub calls: Arc<Mutex<Vec<ChatExecutionRequest>>>, pub response: ChatExecutionResult }`，只用于测试，不读取凭据。
- `pub fn CoreBridge::preflight_chat(&self, principal: &Principal, api_key_id: &str, client_idempotency_key: Option<&str>, body: &Value) -> Result<PreflightResult, CoreError>`。
- `pub fn CoreBridge::settle_chat(&self, reservation_id: &str, outcome: ChatOutcome) -> Result<(), CoreError>`。

- [ ] **Step 1: 写 Core flow 失败测试**

用 Mock executor 验证：创建用户→发行 Key→授予 `chat_request`→预检成功→执行一次 mock→成功结算；预算不足不调用 executor；同一幂等键不产生第二次 reservation；上游不确定结果转 unknown。

```rust
#[test]
fn mock_chat_flow_reserves_before_execution_and_settles_once() {
    let (bridge, principal) = test_bridge_with_policy_and_grant(1);
    let first = bridge.preflight_chat(&principal, "key-1", Some("idem-1"), &chat_body()).unwrap();
    assert_eq!(first.reservation.amount, 1);
    let executor = MockChatExecutor::ok();
    let result = executor.execute(first.execution).unwrap();
    bridge.settle_chat(&first.reservation.id, ChatOutcome::Success(result)).unwrap();
    assert_eq!(executor.calls().len(), 1);
    assert_eq!(bridge.balance("u1", "chat_request").unwrap().available, 0);
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test core_flow -- --nocapture`

预期：因 ports、bridge 和跨表事务尚未实现而失败。

- [ ] **Step 3: 实现 bridge 与 Mock**

`preflight_chat` 先校验 `chat:invoke`，再根据 endpoint/model/body 解析 CostPolicy，调用 `begin_request`，最后调用 `reserve`；任何后续失败都必须带 reservation id。`settle_chat` 将明确失败映射为 release，将成功映射为 commit，将 transport timeout/断连映射为 unknown。Mock executor 只能在测试配置中构造，生产代码不得通过默认值自动使用 Mock。

在 `ApiSharedState` 增加 `pub core: Option<Arc<CoreBridge>>`；`core_mode=off` 时为 `None`，`shadow`/`enforce` 时启动前打开并迁移 SQLite。Core bridge 不直接持有 JWT，不改变现有 `ApiPool` 的凭据生命周期。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test core_flow -- --nocapture`；再运行 `cargo check --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix`。

预期：Core flow PASS，Tauri crate 编译通过。

- [ ] **Step 5: 提交**

```powershell
git add src-core src-tauri/Cargo.toml src-tauri/src/api_server/mod.rs src-tauri/src/api_server/core_bridge.rs
git commit -m "feat: add gateway core bridge and mock port"
```

### Task 6: 接入 Core 鉴权和 scope，保留旧模式兼容

**Files:**
- Modify: `src-tauri/src/api_server/auth.rs`
- Modify: `src-tauri/src/api_server/gateway_settings.rs`
- Modify: `src-tauri/src/api_server/mod.rs`
- Test: `src-tauri/src/api_server/auth.rs`（现有单测扩展）

**Interfaces:**
- `GatewaySettings` 增加 `#[serde(default = "default_core_mode")] pub core_mode: String`，默认返回 `"off"`，旧 JSON 缺字段可正常反序列化。
- `auth::bearer_auth` 在 `enforce` 时插入 `aiwork_core::Principal` 和现有 `KeyId`；`shadow` 只插入观测标记，不写用户 reserve；`off` 完全沿用 `api_keys.json`。
- `auth::require_request_scope(request: &Request, scope: &str) -> Result<(), Response>` 从 Principal 读取 scope，不能从请求体读取用户/权限。

- [ ] **Step 1: 写鉴权失败测试**

扩展现有测试，覆盖：Core enforce 下健康端点仍免鉴权；未携带 Core Key 返回 401；合法 Key 插入正确 `user_id`；body/query/header 的 `user_id` 不改变 Principal；缺少 `models:read` 或 `chat:invoke` 返回 scope 错误；旧 settings JSON 没有 `core_mode` 时加载 `off`。

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix auth::tests -- --nocapture`

预期：新增 Core enforce 测试失败，现有 legacy 测试继续通过。

- [ ] **Step 3: 实现双模式鉴权**

保留现有 `is_public_liveness_path` 和严格的公开素材 content 路径。请求提取 Bearer/x-api-key 后，根据 `state.core` 和 `CoreMode` 选择路径：`enforce` 只调用 `CoreStore::authenticate_api_key`；`shadow` 只做可选 Core lookup 及指标；`off` 使用现有 JSON Key 原子记账。enforce 下不得因为 `auth_disabled=true` 放行 anonymous。

启动时由 `commands/api_server.rs::do_start` 加载 settings，若 mode 不是 off 则 `CoreStore::open(&state.data_dir)?.migrate()?`，失败时拒绝启动而不是退回无账本模式。修改与现有脏 `gateway_settings.rs`/`mod.rs` 合并时只增加字段和初始化，不删除用户已有逻辑。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix auth::tests -- --nocapture`；再运行 `cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix`。

预期：Core/legacy 鉴权测试和现有 Rust 全量测试通过。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/api_server/auth.rs src-tauri/src/api_server/gateway_settings.rs src-tauri/src/api_server/mod.rs src-tauri/src/commands/api_server.rs
git commit -m "feat: add core authentication mode"
```

### Task 7: 接入 Models 与非流式 Chat 的预算闭环

**Files:**
- Modify: `src-tauri/src/api_server/routes.rs`
- Modify: `src-tauri/src/api_server/server.rs`
- Modify: `src-tauri/src/api_server/mod.rs`
- Test: `src-tauri/src/api_server/routes.rs`（新增 Core mode handler tests）

**Interfaces:**
- `routes::models` 在 enforce 时要求 `models:read`，返回现有统一目录格式，不增加未经确认的能力字段。
- `routes::chat_completions` 在 enforce 时只接受 `stream=false`；`stream=true` 返回 HTTP 501、稳定 code `stream_not_enabled_in_phase1`，不创建 reservation、不调用上游。
- enforce 非流式 Chat 需要 `chat:invoke`；如果客户端不提供 `Idempotency-Key` 返回 400 `idempotency_key_required`，避免 Phase 1 无法区分重试。
- `x-request-id` 只用于关联日志，不能代替 Idempotency-Key；Core 生成的 `request_id` 返回到响应头。

- [ ] **Step 1: 写 HTTP/handler 失败测试**

使用测试用 `CoreBridge` 和 Mock executor，覆盖：无 scope 403、无成本策略 400、额度不足 429 `insufficient_quota`、缺幂等键 400、重复请求不第二次调用 mock、stream 501、成功响应含 request id。不要向真实 `AGENT_HOST` 发请求。

```rust
#[tokio::test]
async fn core_chat_reserves_before_dispatch_and_replays_idempotency() {
    let app = test_router_with_mock_core(1);
    let first = post_chat(&app, "key", "idem-1", false).await;
    let second = post_chat(&app, "key", "idem-1", false).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(mock_call_count(&app), 1);
}
```

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix core_chat -- --nocapture`

预期：因 handler 尚未调用 Core bridge 而失败。

- [ ] **Step 3: 实现最小闭环**

在 `chat_completions` 完成 JSON 解析、模型解析和协议检查后、现有 `state.inflight_guard()`/dispatch 之前调用 `CoreBridge::preflight_chat`。保留现有 dispatch/响应投影；将已知成功 usage 转为 `actual_amount`，没有可靠 usage 时传 `None` 让 Core 保守 commit；已确认未发送的客户端错误 release；上游超时/断连使用 unknown。所有 return/error 分支必须通过一个内部结算 helper，避免遗漏 reservation。

在 `models` handler 读取 request Principal 并做 scope 检查；在 `server.rs` 只保留现有 route 定义和 middleware 顺序，不另建绕过 auth 的路径。状态码映射只针对新 Core 模式，legacy 模式保持既有错误格式。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix core_chat -- --nocapture`；再运行 `cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix`。

预期：Core handler tests PASS，现有 300+ Rust 单测无回归；测试输出必须明确为 Mock，不声称真实上游通过。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/api_server/routes.rs src-tauri/src/api_server/server.rs src-tauri/src/api_server/mod.rs
git commit -m "feat: enforce core budget on chat and models"
```

### Task 8: 旧 JSON 检查、管理员命令与安全迁移报告

**Files:**
- Create: `src-tauri/src/api_server/core_migration.rs`
- Create: `src-tauri/src/commands/core.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/commands/api_server.rs`
- Test: `src-core/tests/core_flow.rs`、`src-tauri/src/api_server/core_migration.rs`

**Interfaces:**
- `pub struct LegacyMigrationMapping { pub legacy_key_id: String, pub user_id: String }`。
- `pub struct MigrationReport { pub source_hashes: BTreeMap<String, String>, pub key_count: usize, pub unmapped_keys: Vec<String>, pub asset_count: usize, pub video_count: usize, pub processing_video_count: usize, pub cached_credit_count: usize, pub errors: Vec<String> }`。
- `pub fn inspect_legacy(data_dir: &Path) -> Result<MigrationReport, CoreError>`：只读，不创建用户额度、不写 Key。
- `pub fn apply_legacy(data_dir: &Path, mappings: &[LegacyMigrationMapping], actor_user_id: &str) -> Result<MigrationReport, CoreError>`：事务导入用户/资源映射和历史 observation 标签；无映射记录进入报告并中止写入。
- Tauri commands：`core_status`、`core_migration_inspect`、`core_migration_apply`、`core_user_create`、`core_api_key_issue`、`core_quota_grant`。Key issue 命令返回一次 plaintext，命令日志和前端 store 不持久化它。

- [ ] **Step 1: 写迁移/命令失败测试**

用临时 data dir 创建合法和损坏的 `api_keys.json`、`remaining_credits.json`、`video_tasks.json`、`assets.json`，断言 inspect 只产生报告；未提供 Key→user 映射时 apply 不写额度/资源；`processing` 视频计数进入 reconcile_required；报告和 SQLite 不包含 plaintext key。

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix core_migration -- --nocapture`

预期：因 migration module/commands 不存在而失败。

- [ ] **Step 3: 实现只读检查和明确映射导入**

读取旧 JSON 使用现有 `fs_utils::read_json` 语义，但先保存源文件 SHA-256 和文件存在/损坏状态。`api_keys.json` 的旧明文 Key 不写入 Core；apply 只接受管理员显式映射，并为目标用户发行新的 Core Key，旧 Key 进入迁移遗留禁用报告。`remaining_credits.json` 只写 `source=json_cache` 的历史 observation 表或迁移报告，不生成用户 grant；Phase 1 只有 Core quota grant 能生成用户额度。

把 `processing` 视频、无 owner 映射的 asset、未知状态和重复 id 放入报告并拒绝该批次的部分提交。所有写入带 `actor_user_id` 和理由。Tauri 命令遵守现有命令参数 snake_case/顶层 Rust 参数名约定，注册到 `main.rs` 的 `invoke_handler`。

- [ ] **Step 4: 运行通过测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix core_migration -- --nocapture`；再运行全量 Rust 测试。

预期：合法、损坏、未映射、重复和秘密不泄露测试 PASS。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/api_server/core_migration.rs src-tauri/src/commands/core.rs src-tauri/src/commands/mod.rs src-tauri/src/main.rs src-tauri/src/commands/api_server.rs
git commit -m "feat: add safe core migration and admin commands"
```

### Task 9: Phase 0/1 集成验证、运行手册和交付检查

**Files:**
- Create: `src-core/tests/full_phase1.rs`
- Create: `docs/core-foundation-operations.md`
- Modify: `docs/server-deployment.md`
- Modify: `docs/user-manual.md`
- Test: `src-core/tests/full_phase1.rs`、已有 Rust/Python tests

**Interfaces:**
- 提供一个只使用 Mock executor 的 `run_phase1_smoke()` 测试入口，返回请求 id、reservation id、最终状态、用户余额和 Mock 调用数。
- 文档说明 `core_mode=off|shadow|enforce`、SQLite 备份位置、Key 一次性显示、Core grant 操作、迁移前备份和禁止真实 upstream smoke test。

- [ ] **Step 1: 写端到端 Mock 失败测试**

端到端场景顺序固定为：创建 admin/user → issue key → grant 2 → Chat 成功 1 → 相同 idempotency 重放 → Chat 预算不足 → 上游 timeout 进入 unknown → 重启 Store 读取 unknown → 检查审计事件和余额不超额。断言没有真实 HTTP 上游调用。

- [ ] **Step 2: 运行失败测试**

运行：`cargo test --manifest-path src-core/Cargo.toml --test full_phase1 -- --nocapture`

预期：在最终 smoke harness 尚未建立时失败。

- [ ] **Step 3: 实现 smoke harness 和运维文档**

测试只通过 trait 注入 Mock，禁止读取 `AGENT_HOST`、`remaining_credits.json` 的值作为额度、真实 API Key 或当前运行服务。文档明确旧 JSON 仍保留、Core SQLite 备份使用文件级停机备份、`core_mode=enforce` 前必须有用户/Key/policy/grant 和 parity report。

- [ ] **Step 4: 执行全量验证**

按以下顺序运行并保存输出摘要：

```powershell
npm test
cargo test --manifest-path src-core/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix
python src-python/tests/test_wb_credits.py
python src-python/tests/test_auto_checkin.py
```

只读检查 `git diff --check`、`git status --short`、`git diff --stat HEAD~1`，确认没有 `data/`、凭据、target 或无关用户修改进入提交。不得运行 `src-python/tests/test_api_server.py` 的真实上游路径作为本计划证据。

- [ ] **Step 5: 提交交付文档和测试**

```powershell
git add src-core docs/core-foundation-operations.md docs/server-deployment.md docs/user-manual.md
git commit -m "docs: document core foundation operations"
```

## 实施后的验收清单

- [ ] `src-core` 可独立编译/测试，SQLite Schema 可重复迁移且外键/WAL 已启用。
- [ ] Core Key 只保存 digest/prefix，鉴权 Principal 的用户来源只能是 Key 记录。
- [ ] 并发 reserve 不会超过 grant；release/commit/unknown 和重复结算都有明确结果。
- [ ] 幂等键按 user+endpoint 隔离，hash 冲突返回 409，服务重启后仍可识别。
- [ ] Core enforce 下 Chat 没有 policy/额度/Key/scope 时 fail closed，成功和失败都结算。
- [ ] `stream=true` 在 Phase 1 明确 501，不会绕过预算；legacy 模式行为未回归。
- [ ] 旧 JSON migration 不导入用户额度，不把旧明文 Key 写入 Core，不静默猜 owner。
- [ ] 全量测试只证明 Mock upstream 闭环，不声称真实上游计费/余额或真实生成通过。
- [ ] 现有工作树修改保持原样，所有新提交可单独审查。

## 后续独立计划边界

Phase 1 通过后，再分别生成并审阅以下计划：

1. **Credit-aware scheduler**：账号 observation、upstream lease、并发槽、stale/fail-closed 和真实只读刷新。
2. **Async media lifecycle**：视频/素材用户隔离、流式、取消、任务恢复和重启对账。
3. **Local Core/Agent process split**：独立进程、凭据存储、Tauri 编排、Nginx/FRP 单入口和部署迁移。
4. **Management and observability**：用户/额度/账号/策略/审计 UI、指标、告警、备份恢复和运维审计。

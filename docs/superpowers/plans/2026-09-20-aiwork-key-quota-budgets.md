# AI Work Assistant Key Quota Budgets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有 Core 用户额度和请求生命周期之上，实现 schema v12 的 Key 永久额度、可选用户总上限、同请求双层安全预占/结算、显式旧额度迁移和脱敏管理/查询闭环。

**Architecture:** CoreStore 增加 `quota_budget_accounts` 预算账户目录；`quota_ledger` 与 `quota_reservations` 通过 `budget_account_id`、`event_group_id` 和 Key/用户账户引用表达同一请求的两层约束。Key budget 和 User cap 各自校验并分别记账，但公共用户消费投影只按当前 Principal 的 Key 侧统计，避免双扣。现有请求幂等、上游 lease、视频队列和 recovery 继续作为唯一生命周期入口，路由外不创建第二本账。

**Tech Stack:** Rust 2021, `aiwork-core`, SQLite/rusqlite WAL + `TransactionBehavior::Immediate`, Axum 0.7, Tauri commands, React/TypeScript, Vitest, existing Mock upstream adapters.

**Spec:** `docs/superpowers/specs/2026-09-20-aiwork-key-quota-design.md`

## Global Constraints

- `CURRENT_SCHEMA_VERSION` 从 11 升到 12；v11 用户流水必须保留，不能复制给同一用户的所有 Key。
- `quota_budget_accounts.scope` 只有 `user_cap` 和 `key`；Key 账户的 `api_key_id` 必填且必须属于 `user_id`，用户账户不绑定 Key。
- work/general 等 `resource_kind` 必须分别记账，不跨类别折算、挪用或自动改用另一类。
- 一个请求只能有一个 request/reservation/event group；User cap 与 Key 约束不是两次实际消费。
- Key budget 未配置、预算版本失效、旧 held/unknown 未完成迁移时，Core enforce 必须 fail-closed。
- 不推断 Trae、WorkBuddy 或其他上游的真实单价、扣费顺序、退款规则；上游 observation 不进入用户 Key 账本。
- 所有新测试、Cargo target、日志和临时数据库使用 `D:\\gpt`；命令显式设置 `$env:TEMP='D:\\gpt'; $env:TMP='D:\\gpt'`，不在 C 盘建立测试产物。
- 保留工作区已有用户改动；只提交本计划明确新增/修改的文件，不暂存 `.gitignore`、现有文档/脚本改动、`src-core/Cargo.lock`、`src-tauri/src/api_server/trae_resource_upload.rs` 或 `src-tauri/target-fix/`。
- 不把明文 API Key、digest、JWT、Cookie、prompt、完整上游响应、上游账号凭据写入普通用户投影、日志或管理响应。
- 只使用认证 middleware 注入的 `Principal` 识别用户和 Key；请求 body、query、Host、普通转发 header 不能覆盖身份。

## Review Focus

- **跨 Key 越权**：Key A 的额度不能被 Key B 消耗，Key budget 的管理、查询和 reservation 必须同时验证 Key 所属用户；由 Task 2 和 Task 3 的真实 SQLite 测试覆盖。
- **v11 迁移污染**：已有用户额度不能自动复制到每个 Key，旧 held/unknown reservation 不能被静默换 Key 或退款，迁移失败必须全量回滚；由 Task 1 的迁移夹具覆盖。
- **双层重复扣费**：同一 `event_group_id` 的 User cap 与 Key 事件不能让公共 Key/用户投影重复累计；由 Task 3 的 commit/release/unknown 测试覆盖。
- **并发与幂等**：两个 Key 并发预占不能超过 User cap；同一 idempotency key 同参只能创建一个 reservation，重复 settle/release/reconcile 不重复写消费；由 Task 3 和 Task 4 的并发测试覆盖。
- **公开边界**：普通用户只能得到当前 Principal 的 Key 投影；缺 Key 预算、缺 scope、Core off/shadow、迁移未完成和其他用户查询都必须返回稳定错误且不泄露余额；由 Task 4 和 Task 5 覆盖。

### Task 1: Schema v12、预算账户模型和旧数据迁移

**Files:**
- Modify: `src-core/src/schema.rs`（增加 v12 预算账户表、索引和新增列声明）
- Modify: `src-core/src/store.rs`（`CURRENT_SCHEMA_VERSION`、v11→v12 migration 分支和 backfill）
- Modify: `src-core/src/models.rs`（预算账户、预算余额、迁移状态和 Key grant 输入类型）
- Modify: `src-core/src/lib.rs`（导出新增 Core 类型）
- Test: `src-core/tests/schema_bootstrap.rs`、`src-core/tests/migration.rs`

**Interfaces:**
- Consumes: 当前 `CoreStore::migrate`, `SCHEMA_V11`, `users`, `api_keys`, `quota_ledger`, `quota_reservations`, `requests`。
- Produces: `CURRENT_SCHEMA_VERSION == 12`；`quota_budget_accounts` 及唯一索引；`QuotaBudgetScope`, `QuotaMigrationState`, `QuotaBudgetAccount`, `QuotaBudgetBalance`, `KeyQuotaGrant`, `LegacyQuotaAllocation`；`CoreStore::migrate_v11_to_v12` 只在 `migrate()` 内调用。

预算账户表的最终 SQL 结构固定为：

```sql
CREATE TABLE quota_budget_accounts (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL CHECK(scope IN ('user_cap','key')),
  user_id TEXT NOT NULL REFERENCES users(id),
  api_key_id TEXT REFERENCES api_keys(id),
  resource_kind TEXT NOT NULL,
  enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
  version INTEGER NOT NULL CHECK(version > 0),
  migration_state TEXT NOT NULL CHECK(migration_state IN ('ready','legacy_unassigned','reconcile_required')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  CHECK((scope = 'user_cap' AND api_key_id IS NULL) OR
        (scope = 'key' AND api_key_id IS NOT NULL))
);
CREATE UNIQUE INDEX quota_budget_accounts_user_cap_uq
  ON quota_budget_accounts(user_id, resource_kind)
  WHERE scope = 'user_cap';
CREATE UNIQUE INDEX quota_budget_accounts_key_uq
  ON quota_budget_accounts(user_id, api_key_id, resource_kind)
  WHERE scope = 'key';
```

`quota_ledger` 增加可空的 `budget_account_id`, `event_group_id`, `api_key_id`, `budget_version`；`quota_reservations` 增加可空的 `api_key_id`, `key_budget_account_id`, `user_cap_account_id`, `event_group_id`。先建预算账户表再 `ALTER TABLE`，避免新增外键列引用不存在的表。

- [ ] **Step 1: Write the failing migration tests**

在 `schema_bootstrap.rs` 增加 `schema_v12_creates_budget_account_directory_and_indexes`：创建新 CoreStore 后 migrate，断言版本为 12、四个新 reservation/ledger 列存在、预算表和两个 partial unique index 存在、外键开启。

在 `migration.rs` 增加 `v11_user_ledger_is_backfilled_once_without_key_copy`：用 SQLite 建立带 `schema_version=11` 的最小 v11 数据，写入两个用户额度流水、一个 active API Key 和一条旧 reservation；运行 `CoreStore::migrate()`，断言每个用户/resource 只有一个 `user_cap` 账户、旧流水指向该账户、没有自动创建 `key` 账户、未结束 reservation 标为 `reconcile_required`/迁移待处理而未改绑其他 Key。

增加 `v12_migration_failure_rolls_back_budget_tables_and_columns`：在 v11 数据中插入违反 backfill 约束的 Key/用户关系，运行迁移并断言返回错误，schema 仍为 11，预算表、预算列和部分流水都不存在。

- [ ] **Step 2: Run the focused tests to verify red**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task1-red --offline --locked --test schema_bootstrap schema_v12_creates_budget_account_directory_and_indexes -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task1-red --offline --locked --test migration v11_user_ledger_is_backfilled_once_without_key_copy -- --nocapture
```

Expected: FAIL because schema version remains 11 and the budget account table/columns do not exist.

- [ ] **Step 3: Implement the minimal v12 schema and migration**

在 `schema.rs` 增加 v12 表结构常量；在 `store.rs` 设置 `CURRENT_SCHEMA_VERSION: u32 = 12`，让所有旧版本 migration 分支在完成 v10→v11 后继续调用 `migrate_v11_to_v12`，并让 `12 => harden_v6_records` 保持可重复打开。

`migrate_v11_to_v12` 按以下顺序在当前 Immediate transaction 内执行：建预算账户表和 partial indexes；增加 ledger/reservation 列；从 `quota_ledger` 与 `quota_reservations` 的 distinct `user_id + resource_kind` 建 user_cap 账户；将旧 ledger 指向 user_cap 并使用 `legacy-<entry_id>` 事件组；根据 `requests.api_key_id` 只记录旧 reservation 的原始 Key 归属，不创建新的 Key 账户，不自动退款；有 held/unknown 旧 reservation 的 user_cap 账户标为 `reconcile_required`，其余有旧余额的账户标为 `legacy_unassigned`；最后把 schema_meta 更新为 12。任何 SQL/约束错误都通过事务回滚。

新增模型必须只使用稳定字符串序列化 scope/state，且 `KeyQuotaGrant` 强制包含 `api_key_id`, `resource_kind`, `amount`, `actor_user_id`, `reason`；`LegacyQuotaAllocation` 强制包含 `source_user_id`, `api_key_id`, `resource_kind`, `amount`, `actor_user_id`, `reason`。

- [ ] **Step 4: Run the focused migration tests to verify green**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task1-green --offline --locked --test schema_bootstrap --test migration -- --nocapture
```

Expected: v12 schema、backfill、外键和失败回滚测试通过，既有 migration 测试不改变语义。

- [ ] **Step 5: Commit Task 1**

```powershell
git add src-core/src/schema.rs src-core/src/store.rs src-core/src/models.rs src-core/src/lib.rs src-core/tests/schema_bootstrap.rs src-core/tests/migration.rs
git diff --cached --check
git commit -m "feat: migrate core quota schema to v12"
```

### Task 2: 预算账户查询、发放与显式 legacy 分配

**Files:**
- Modify: `src-core/src/quota.rs`（预算账户查找、Key grant/balance、legacy allocation）
- Modify: `src-core/src/store.rs`（账户归属验证和管理员审计辅助）
- Modify: `src-core/src/error.rs`（预算未配置、迁移待处理、Key 归属和版本错误）
- Modify: `src-core/src/models.rs`（余额/投影返回类型）
- Test: `src-core/tests/quota.rs`、`src-core/tests/identity.rs`

**Interfaces:**
- Consumes: Task 1 的 `quota_budget_accounts`、`KeyQuotaGrant`、`LegacyQuotaAllocation`、`Principal` 和 `authorize_admin_principal`。
- Produces: `CoreStore::key_quota_grant_as_admin(&self, principal: &Principal, input: KeyQuotaGrant) -> Result<QuotaBudgetBalance, CoreError>`；`CoreStore::key_quota_balance_as_admin(&self, principal: &Principal, api_key_id: &str, resource_kind: &str) -> Result<QuotaBudgetBalance, CoreError>`；`CoreStore::key_quota_allocate_legacy_as_admin(&self, principal: &Principal, input: LegacyQuotaAllocation) -> Result<QuotaBudgetBalance, CoreError>`。

`QuotaBudgetBalance` 至少返回 `api_key_id`, `user_id`, `resource_kind`, `available`, `held`, `settled`, `version`, `enabled`, `migration_state`, `key_quota_configured`；管理端可以看到 Key prefix，但 CoreStore 不返回 plaintext/digest。

- [ ] **Step 1: Write the failing repository tests**

在 `quota.rs` 测试夹具中创建 admin、user、Key A、Key B，并断言：

```rust
let a = store.key_quota_grant_as_admin(&admin, KeyQuotaGrant { api_key_id: key_a.id.clone(), resource_kind: "chat_request".into(), amount: 10, actor_user_id: "admin".into(), reason: "initial".into() })?;
assert_eq!(a.available, 10);
assert!(matches!(store.key_quota_balance_as_admin(&user, &key_a.id, "chat_request"), Err(CoreError::AdminRequired)));
assert!(matches!(store.key_quota_grant_as_admin(&admin, KeyQuotaGrant { api_key_id: key_b.id.clone(), resource_kind: "chat_request".into(), amount: 1, actor_user_id: "user".into(), reason: "forged".into() }), Err(CoreError::AdminRequired)));
```

增加 `key_grant_rejects_cross_user_key_and_non_positive_amount`、`legacy_allocation_moves_once_without_copying`：管理员只能向实际归属用户的 Key 发放，legacy 分配在同一 event group 写入 user_cap 负向 adjust 与 Key 正向 adjust，第二次使用同一迁移标识必须幂等而不能再次增加 Key 余额。

- [ ] **Step 2: Run the repository tests to verify red**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task2-red --offline --locked --test quota key_grant_rejects_cross_user_key_and_non_positive_amount -- --nocapture
```

Expected: FAIL because the Key budget APIs and return type do not exist.

- [ ] **Step 3: Implement budget lookup and admin operations**

在 Immediate transaction 中先调用 `authorize_admin_principal`，再查询 `api_keys` 的 `id,user_id,status`；禁止 revoked/disabled Key，禁止 `principal.user_id` 伪造管理员身份。Key grant 首次创建 `scope='key'` 的 ready 账户，后续同 Key/resource 使用同一账户并递增 `version`；正数 grant 写一条 append-only `adjust`，`delta=amount`，`event_group_id='admin-<entry_id>'`，并写脱敏 audit event。

legacy allocation 必须同时锁定 source user_cap 与目标 Key account，检查 source 的 `migration_state='legacy_unassigned'`、余额不少于 amount、目标 Key 属于同一用户，使用稳定 `migration_id` 作为 event group；在一笔事务内写 source `delta=-amount` 和 target `delta=amount`，将 source/target 状态更新为 ready，重复 migration_id 返回原结果且不重复记账。

`key_quota_balance_as_admin` 只允许管理员读取指定 Key，统计 available=`SUM(delta)-SUM(held/committed key events)`、held=`held+unknown`、settled=`commit amount`，并返回 migration state；缺账户返回 `KeyQuotaNotConfigured`，不回退到用户级余额。

- [ ] **Step 4: Run Core quota tests and full Core targets**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task2-green --offline --locked --test quota --test identity -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task2-green --offline --locked --tests
```

Expected: Key ownership、admin auth、legacy allocation 幂等和既有 Core 测试全部通过。

- [ ] **Step 5: Commit Task 2**

```powershell
git add src-core/src/quota.rs src-core/src/store.rs src-core/src/error.rs src-core/src/models.rs src-core/tests/quota.rs src-core/tests/identity.rs
git diff --cached --check
git commit -m "feat: add core key quota administration"
```

### Task 3: 双层 reserve/settle/release/unknown 与并发幂等

**Files:**
- Modify: `src-core/src/quota.rs`（双层账户余额和账本事件）
- Modify: `src-core/src/requests.rs`（`preflight_reserve` 和 reservation 查询/结算入口）
- Modify: `src-core/src/models.rs`（双层 reservation 结果和事件组字段）
- Modify: `src-core/src/error.rs`（双层预算错误）
- Test: `src-core/tests/quota.rs`、`src-core/tests/requests.rs`、`src-core/tests/core_flow.rs`

**Interfaces:**
- Consumes: Task 1 的 reservation columns、Task 2 的 Key/User cap account lookup、现有 `PreflightReserveInput`, `Settlement`, `ReserveResult` 和 `idempotency_keys`。
- Produces: `PreflightReserveResult::Created` 返回带 `event_group_id`, `key_budget_account_id`, `user_cap_account_id` 的同一 reservation；现有 `settle`, `settle_request`, `reservation_for_request` 保持调用方兼容但同时处理两层账户。

- [ ] **Step 1: Write the failing dual-layer tests**

在 `requests.rs`/`quota.rs` 增加真实 SQLite 测试：

1. Key A 有 5、Key B 有 5、同一用户 User cap 有 6；两个不同 idempotency key 并发预占各 4 时只有一个成功，User cap held 不超过 4，失败方不留下 request/reservation/ledger 残留。
2. 无 User cap 时 Key budget 独立成功；Key A 不消耗 Key B。
3. 同一 idempotency key 同参返回同一 request/reservation，同键异参返回 Conflict。
4. Commit、Release、Unknown、重复 settle/release/reconcile 都只产生一次对应的 event group 结果；Key 侧公共 settled 只累计一次，User cap 不被投影重复相加。
5. 禁用/撤销 Key、新建请求、预算 migration_state 非 ready、版本失效都在写入前拒绝。

- [ ] **Step 2: Run the dual-layer tests to verify red**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task3-red --offline --locked --test requests dual_layer_reservation_enforces_user_cap_across_keys -- --nocapture
```

Expected: FAIL because existing preflight only consults `user_id` and has no Key account/event group fields.

- [ ] **Step 3: Implement the dual-layer reserve transaction**

在 `preflight_reserve` 的同一个 `TransactionBehavior::Immediate` transaction 内按固定顺序校验 active Key、幂等记录、CostPolicy、Key account 和 optional User cap。对每层读取 `available = grants + adjusts - committed - held - unknown`，不足即返回统一 `insufficient_quota` 且不插入任何 request/reservation/ledger。

成功时生成一个 `event_group_id`，创建一个 `quota_reservations` row，写 Key `reserve` 和可选 User cap `reserve` ledger entries；两个 entry 使用相同 request_id/event_group_id，Key/User account id 各自明确。保留现有 request state transition 和 idempotency scope，不创建第二个 request。

把 `settle_impl` 改为根据 reservation state 先判断幂等，再在同一事务中对两层追加 commit/release/unknown 事件并更新 reservation；`actual_amount` 只能小于等于预占额，unknown 保留 hold；重复相同决策返回已应用结果，冲突决策返回稳定状态错误。旧的 `quota.reserve()`/`grant()` 直接用户 API 继续可用，但新请求生命周期只能走 dual-layer path。

- [ ] **Step 4: Run focused tests, then the complete Core suite**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task3-green --offline --locked --test requests --test quota --test core_flow -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task3-green --offline --locked --tests
```

Expected: 双 Key 并发不超过 User cap，所有重复通知幂等，所有既有 request/lease/quota 测试通过。

- [ ] **Step 5: Commit Task 3**

```powershell
git add src-core/src/quota.rs src-core/src/requests.rs src-core/src/models.rs src-core/src/error.rs src-core/tests/quota.rs src-core/tests/requests.rs src-core/tests/core_flow.rs
git diff --cached --check
git commit -m "feat: enforce dual-layer core quota reservations"
```

### Task 4: Core bridge、视频队列与 recovery 接入双层预算

**Files:**
- Modify: `src-tauri/src/api_server/core_bridge.rs`（chat/non-stream preflight、settlement 映射）
- Modify: `src-tauri/src/api_server/core_stream.rs`（流式 lifecycle 的 reservation/event group）
- Modify: `src-tauri/src/api_server/core_video.rs`、`src-tauri/src/api_server/routes.rs`（视频提交、取消、unknown 和释放）
- Modify: `src-tauri/src/api_server/scheduler.rs`（启动 recovery/reconcile 一致性检查）
- Test: `src-tauri/src/api_server/core_bridge.rs`、`src-tauri/src/api_server/routes.rs`、`src-tauri/src/api_server/scheduler.rs`

**Interfaces:**
- Consumes: Task 3 的双层 `PreflightReserveResult`、现有 mock chat/video executor、lease settlement 和 durable queue 状态机。
- Produces: chat、stream、video 使用同一个 Core reservation；lease 失败释放两层、成功/失败按一次 event group 结算、transport unknown 保留两层 held 并标记 reconcile。

- [ ] **Step 1: Write the failing integration tests**

增加以下测试：

- `core_chat_key_quota_rejects_before_mock_dispatch`：当前 Key 余额不足时 mock executor 调用次数为 0，Core 返回 `insufficient_quota`。
- `core_chat_user_cap_is_shared_by_two_keys`：两个 Principal/Key 进入同一个 store，并发/顺序请求总预占不超过 User cap。
- `core_stream_and_video_repeated_settlement_keeps_one_event_group`：stream/video 成功、显式拒绝、cancel、unknown 各重复通知一次，reservation 和两层账本只改变一次。
- `scheduler_recovery_marks_mismatched_quota_event_group_reconcile_required`：启动发现缺少一侧 ledger 或旧 unknown reservation，保留 held 并进入 reconcile_required，不自动退款、不换账号重放。

- [ ] **Step 2: Run the integration tests to verify red**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task4-red --offline --locked core_chat_key_quota_rejects_before_mock_dispatch -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task4-red --offline --locked core_stream_and_video_repeated_settlement_keeps_one_event_group -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task4-red --offline --locked scheduler_recovery_marks_mismatched_quota_event_group_reconcile_required -- --nocapture
```

Expected: FAIL because the bridge currently calls the user-only reservation/settlement projection and does not carry event-group consistency through stream/video recovery.

- [ ] **Step 3: Implement the bridge and recovery wiring**

保持 `CoreBridge::preflight_chat_with_lease_for_accounts`、`preflight_video_with_lease` 和 stream adapter 的 public call shape；内部把 reservation amount/IDs 交给 Task 3 的 dual-layer methods。所有 `settle_core_*` 分支统一调用一次 `settle_request`，不在路由和 adapter 各自扣费。视频 heartbeat/cancel 只推进 job/attempt/lease 状态，最终预算释放由 reservation settlement 决定。

在 scheduler startup recovery 中增加 event group 完整性检查：两层事件缺任一、账户版本不一致或上游状态 unknown 时，只写 audit/reconcile_required 标志并保留预算；明确 accepted=false 的上游失败才释放两层；不得 fallback 到 legacy pool、自动换 Key 或重放。

- [ ] **Step 4: Run focused Tauri tests and the full Tauri suite**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:AIWORK_ASSET_DIR=''; $env:AIWORK_VIDEO_DIR=''; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task4-green --offline --locked core_chat_key_quota_rejects_before_mock_dispatch core_stream_and_video_repeated_settlement_keeps_one_event_group scheduler_recovery_marks_mismatched_quota_event_group_reconcile_required -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:AIWORK_ASSET_DIR=''; $env:AIWORK_VIDEO_DIR=''; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task4-green --offline --locked -- --nocapture
```

Expected: focused lifecycle tests and full Tauri suite pass without real upstream calls or C-drive test artifacts.

- [ ] **Step 5: Commit Task 4**

```powershell
git add src-tauri/src/api_server/core_bridge.rs src-tauri/src/api_server/core_stream.rs src-tauri/src/api_server/core_video.rs src-tauri/src/api_server/routes.rs src-tauri/src/api_server/scheduler.rs
git diff --cached --check
git commit -m "feat: connect key budgets to core request lifecycle"
```

### Task 5: Public usage projection、Tauri 管理 API 和 Core Admin Panel

**Files:**
- Modify: `src-core/src/quota.rs`、`src-core/src/models.rs`（当前 Principal 的 Key/user-cap usage projection）
- Modify: `src-tauri/src/api_server/core_account.rs`（`/v1/usage` 新字段和稳定错误）
- Modify: `src-tauri/src/commands/core.rs`、`src-tauri/src/main.rs`（Key quota grant/balance/legacy allocation commands）
- Modify: `src/lib/tauri.ts`、`src/types.ts`（IPC 类型和调用）
- Modify: `src/components/api/CoreAdminPanel.tsx`（Key 额度筛选、发放、迁移、查看）
- Test: `src-tauri/src/api_server/core_account.rs`、`src-tauri/src/commands/core.rs`、`src-tauri/src/main.rs`、`src/components/api/CoreAdminPanel.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 Key admin methods、Task 3 的 balances/event groups、Task 4 的 current Principal lifecycle。
- Produces: `/v1/usage` 保留 `object`/`limit` 外层字段，`balances.available` 为 effective available，`held`/`settled` 为当前 Key 投影，并返回 `key_available`, `user_cap_available`, `key_quota_configured`；缺配置/迁移未完成返回 409 稳定错误。

- [ ] **Step 1: Write the failing API, command and UI tests**

在 `core_account.rs` 增加测试：当前 Principal 只能看到自己的 Key ledger；`effective_available=min(key_available,user_cap_available)`；无 User cap 时使用 Key available；未配置 Key 不返回 legacy user balance 而是 `409 key_quota_not_configured`；off/shadow、缺 scope、其他 Key 不泄露。

在 `commands/core.rs` 增加真实 Store 测试：非 admin、伪造 admin user_id、跨用户 Key、空 reason、非正 amount、重复 legacy migration 都被拒绝或幂等；管理结果不含 plaintext/digest。

在 `CoreAdminPanel.test.tsx` 增加行为测试：选择用户和 Key 后显示 Key prefix、scope、迁移状态和三类余额；发放/迁移调用正确 IPC；页面不渲染 plaintext、digest 或上游账号字段。

- [ ] **Step 2: Run focused tests to verify red**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task5-red --offline --locked usage_ core_ -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:npm_config_cache='D:\gpt\npm-cache'; npm test -- --run src/components/api/CoreAdminPanel.test.tsx
```

Expected: FAIL because the public payload lacks Key fields, commands are not registered and UI has no Key budget actions.

- [ ] **Step 3: Implement public and admin surfaces**

扩展 `CoreQuotaUsageView`/`usage_payload`，保持现有 `object` 和 `limit`，将 `ledger` 限定为当前 Principal 的 Key-scope 脱敏事件；Key 缺失、migration_state 非 ready、预算版本失效分别映射为稳定 409 错误，不回退 legacy user balance。

在 `commands/core.rs` 增加 `core_key_quota_grant`, `core_key_quota_balance`, `core_key_quota_allocate_legacy` 三个 Tauri commands，全部调用真实 admin Principal；在 `main.rs` 的 `generate_handler!` 注册；在 `tauri.ts`、`types.ts` 增加完整请求/响应类型。CoreAdminPanel 保持现有用户/Key筛选，增加 Key resource_kind 预算操作，成功后刷新 Key 列表和 balance，管理 API Key 只存内存。

- [ ] **Step 4: Run focused tests and full frontend/Tauri suites**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-key-quota-task5-green --offline --locked usage_ core_ -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:npm_config_cache='D:\gpt\npm-cache'; npm test -- --run
```

Expected: Key usage API、admin auth、UI behavior 和既有 Tauri/前端测试通过；不调用真实上游。

- [ ] **Step 5: Commit Task 5**

```powershell
git add src-core/src/quota.rs src-core/src/models.rs src-tauri/src/api_server/core_account.rs src-tauri/src/commands/core.rs src-tauri/src/main.rs src/lib/tauri.ts src/types.ts src/components/api/CoreAdminPanel.tsx src/components/api/CoreAdminPanel.test.tsx
git diff --cached --check
git commit -m "feat: expose key quota management and usage"
```

### Task 6: 运维/用户文档、全量验证与最终审查

**Files:**
- Modify: `docs/superpowers/specs/2026-09-20-aiwork-key-quota-design.md`（记录实现状态和验证证据）
- Modify: `docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-operations.md`（补充双层预算 recovery/runbook）
- Modify: `docs/user-manual.md`、`docs/server-deployment.md`、`docs/lan-frp-nginx-design.md`（配置、迁移前置条件、公共边界）
- Test: Core/Tauri/frontend 全量命令、文档契约检查、`git diff --check`

**Interfaces:**
- Consumes: Tasks 1–5 的 schema、API 错误、管理命令、recovery 和 public usage contract。
- Produces: 可操作的 v12 迁移/回滚说明、Core enforce 前置检查、双层余额/unknown/reconcile 观测说明和实际验证记录。

- [ ] **Step 1: Write the documentation contract check first**

使用 PowerShell 只读检查以下标记同时存在于目标文档：`schema v12`, `quota_budget_accounts`, `key_quota_not_configured`, `event_group_id`, `legacy_unassigned`, `unknown`, `reconcile_required`, `上游余额不转换为用户额度`, `D:\\gpt`。检查缺失时退出非零。

- [ ] **Step 2: Run the check to verify red**

将输出保存到 `D:\\gpt\\aiwork-key-quota-docs-red.log`，预期至少缺少实现状态/回滚/Key quota marker；不写 C 盘。

- [ ] **Step 3: Update the operational and user documentation**

写明：v11→v12 是单事务迁移；旧额度不复制；管理员先显式分配 legacy；未完成迁移前 enforce fail-closed；Key/user/upstream 三类余额分开；unknown 只进入 reconcile；公网、LAN、本机共用 Core；Nginx/FRP 不决定权限；不宣称真实上游价格或计费规则。

- [ ] **Step 4: Run all verification commands with fresh output**

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:CARGO_TARGET_DIR='D:\gpt\aiwork-final-core-target'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --offline --locked --tests
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:CARGO_TARGET_DIR='D:\gpt\aiwork-final-tauri-target'; $env:AIWORK_ASSET_DIR=''; $env:AIWORK_VIDEO_DIR=''; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --offline --locked -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; $env:npm_config_cache='D:\gpt\npm-cache'; npm test -- --run
git diff --check
```

逐条读取完整输出，记录通过/失败数量；若失败，按 systematic-debugging 建立回归测试后修复，不把旧失败或未运行命令写成通过。确认 `git diff --cached --name-only` 只包含本任务路径，已有用户脏文件保持未暂存。

- [ ] **Step 5: Commit Task 6**

```powershell
git add docs/superpowers/specs/2026-09-20-aiwork-key-quota-design.md docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-operations.md docs/user-manual.md docs/server-deployment.md docs/lan-frp-nginx-design.md
git diff --cached --check
git commit -m "docs: document key quota migration and recovery"
```

## Execution Notes

- 任务必须按顺序执行；Task 2 不能在 Task 1 migration contract 通过前开始，Task 3 不能在 Key/User account lookup 通过前开始。
- 当前仓库位于普通 `main` checkout 且有用户脏文件；用户已明确“开始”并授权实现，因此在当前 checkout 执行，所有提交都严格路径限定，不使用 destructive git 命令。该决定的代价是实现分支与已有用户改动共存，提交前必须逐项检查 staged path。
- 本计划采用当前会话内执行，不调用不存在的子代理工具；每个 Task 按 RED→GREEN→全量回归→路径限定提交执行。
- 最终审查必须重新对照本计划 Review Focus、书面规范和 ledger；只有新鲜测试输出证明要求后，才可声称方案 1 最小闭环完成。

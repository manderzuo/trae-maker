# AI Work Assistant Phase 2 Credit-Aware Scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 在现有 Core 额度闭环之上实现可恢复的多账号积分/能力观测、原子上游 lease、并发槽和非流式 Chat 的 fail-closed 调度，同时保留 off/shadow 兼容路径。

**Architecture:** 为 src-core 增加 schema v6 的 upstream_accounts、追加式 upstream_observations 和 upstream_leases，由 Core 事务决定 enforce 模式下的账号与 lease。Tauri 先通过 port 提供内置 Trae/WB observation reader 和 upstream executor，ApiPool 仅作为 off/shadow 的兼容实现；Phase 4 再把相同 port 拆到 Local Agent 进程。

**Tech Stack:** Rust 1.77 src-core crate、SQLite/rusqlite、Tauri 2 + axum、serde/serde_json、现有 Tauri vault、现有 Trae/WB 只读积分查询、Mock observation reader/executor；不新增网络依赖，不在测试中调用真实上游生成接口。

**Spec:** docs/superpowers/specs/2026-09-19-aiwork-credit-aware-scheduler-design.md

## Global Constraints

- Core 时间统一为 UTC 整数毫秒；用户 quota、lease 预算和并发计数使用整数逻辑单位。
- 上游积分观测与用户永久额度分表、分语义、分结算路径；remaining_credits.json 和 WorkBuddy cache 只能作为历史/诊断 observation，不能生成 Core grant。
- scheduler_mode 默认 off；off 不改变现有 ApiPool；shadow 不阻断请求、不扣用户 quota；enforce 没有可信 scheduler 时 fail closed，不能回退旧池。
- Core 不保存 JWT、Cookie、refresh token、完整上游响应、prompt 或完整输出；只保存 opaque account_ref/credentials_ref、脱敏摘要和审计元数据。
- 观测失败保留最后成功记录但标记 stale/failed；不能把失败写成零余额。enforce 只接受 policy 允许的 fresh observation。
- lease 的账号并发槽来自持久化 upstream_leases，不能用进程内 inflight 替代；unknown lease 不能因 TTL 自动释放。
- 测试只使用 Mock reader/executor/fixture；禁止读取 AGENT_HOST、真实 WorkBuddy billing URL、真实 API Key、真实运行服务或真实账号余额作为测试前提。
- 保留现有用户 dirty 文件；只暂存任务列出的文件。不得提交 data/、凭据、target/、生成的 Cargo.lock 或用户已有修改。
- 不在本计划实现流式、视频、素材、取消、独立 Agent 进程、管理 UI 或公网部署；未接入 scheduler 的 enforce endpoint 必须返回稳定的 501/scheduler_endpoint_not_enabled。

## Review Focus

1. 同一 max_concurrency=1 账号的并发 acquire 只能成功一次，且失败事务不留下 quota/lease 孤儿；由 Task 2 的并发与回滚测试覆盖。
2. stale、failed、json_cache 观测不能在 enforce 中被选择，但 shadow 仍可形成诊断；由 Task 3/4 的 freshness tests 覆盖。
3. 请求体伪造 user_id、account_ref、allowed/dedicated 约束不能改变 Core 的 Principal 和 lease；由 Task 2/5 的身份与路由 tests 覆盖。
4. timeout、断连、进程异常和过期 lease 必须进入 unknown，不得自动 release 或重复执行；由 Task 2/6/7 的状态机与重启 tests 覆盖。
5. provider/region/capability、Work/General resource 和不同 value scale 不能误混；由 Task 1/3/7 的候选过滤与 adapter contract tests 覆盖。

---

### Task 1: 建立 upstream 目录、观测与 lease 的 Schema v6

**Files:**
- Create: src-core/src/upstream.rs
- Modify: src-core/src/schema.rs
- Modify: src-core/src/store.rs
- Modify: src-core/src/models.rs
- Modify: src-core/src/error.rs
- Modify: src-core/src/lib.rs
- Test: src-core/tests/schema_bootstrap.rs
- Test: src-core/tests/upstream_schema.rs

**Interfaces:**
- Consumes: v5 CoreStore::open/migrate、现有 upstream_observations、Principal、CoreError。
- Produces: UpstreamAccount、UpstreamObservation、UpstreamLease、LeaseState、ObservationStatus、RegisterUpstreamAccount、CoreStore::upsert_upstream_account、CoreStore::append_upstream_observation、CoreStore::get_latest_observation、CoreStore::list_recoverable_leases。

- [ ] Step 1: Write the failing migration and constraint tests

~~~
#[test]
fn migrates_v5_to_v6_without_importing_user_quota_or_secrets() {
    let store = fixture_at_schema_v5();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 6);
    assert!(store.table_exists("upstream_accounts").unwrap());
    assert!(store.table_exists("upstream_leases").unwrap());
    assert_eq!(store.count_rows("quota_ledger").unwrap(), 0);
}

#[test]
fn lease_and_observation_constraints_reject_invalid_values() {
    let store = fresh_store();
    let err = store.append_upstream_observation(invalid_observation()).unwrap_err();
    assert!(matches!(err, CoreError::Validation { .. }));
}
~~~

The test must also assert that credentials_ref is non-empty but its value never appears in any observation summary or audit row, that unknown is a valid lease state, and that repeated migrate is idempotent.

- [ ] Step 2: Run the focused tests and verify failure

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test upstream_schema -- --nocapture

Expected: FAIL because schema version 6 and the upstream data types/methods do not exist.

- [ ] Step 3: Implement schema v6 and typed models

Add SCHEMA_V6 and migrate_v5_to_v6 with upstream_accounts, upstream_leases, indexes, and the value_scale/status columns on upstream_observations:

~~~
CREATE TABLE upstream_accounts (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  credentials_ref TEXT NOT NULL,
  region TEXT,
  capabilities_json TEXT NOT NULL,
  enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
  max_concurrency INTEGER NOT NULL CHECK(max_concurrency > 0),
  state TEXT NOT NULL CHECK(state IN ('available','cooling','forbidden','disabled')),
  cooldown_until_ms INTEGER,
  cooldown_reason TEXT,
  consecutive_errors INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE upstream_leases (
  id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL REFERENCES requests(id),
  account_ref TEXT NOT NULL REFERENCES upstream_accounts(id),
  resource_kind TEXT NOT NULL,
  predicted_units INTEGER NOT NULL CHECK(predicted_units > 0),
  state TEXT NOT NULL CHECK(state IN ('held','active','succeeded','failed','unknown','released')),
  lease_expires_at_ms INTEGER NOT NULL,
  reconcile_until_ms INTEGER,
  upstream_request_ref TEXT,
  error_kind TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  settled_at_ms INTEGER,
  UNIQUE(request_id, resource_kind)
);
~~~

Use explicit SQL column names and typed Rust constructors. summary_json is validated JSON with a bounded serialized length; no model stores raw credentials. The migration must run inside the existing transaction and update CURRENT_SCHEMA_VERSION only after all tables, indexes and checks succeed.

- [ ] Step 4: Run the focused tests and the existing Core suite

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test upstream_schema -- --nocapture

Then run: cargo test --manifest-path src-core/Cargo.toml --offline

Expected: new schema tests and all existing Core tests pass.

- [ ] Step 5: Commit the schema unit

~~~powershell
git add src-core/src/upstream.rs src-core/src/schema.rs src-core/src/store.rs src-core/src/models.rs src-core/src/error.rs src-core/src/lib.rs src-core/tests/schema_bootstrap.rs src-core/tests/upstream_schema.rs
git commit -m "feat: add upstream account observation and lease schema"
~~~

### Task 2: Implement atomic scheduler lease acquisition and settlement

**Files:**
- Modify: src-core/src/upstream.rs
- Modify: src-core/src/store.rs
- Modify: src-core/src/quota.rs
- Modify: src-core/src/requests.rs
- Modify: src-core/src/lib.rs
- Test: src-core/tests/upstream_leases.rs
- Test: src-core/tests/quota.rs

**Interfaces:**
- Consumes: Task 1 models/tables, existing PreflightReserveInput, cost policy lookup, quota reservation settlement and request idempotency.
- Produces: SchedulerLeaseRequest, UpstreamLeaseGrant, LeaseOutcome, CoreStore::preflight_reserve_with_lease, CoreStore::heartbeat_upstream_lease, CoreStore::settle_upstream_lease, CoreStore::recover_expired_upstream_leases。

- [ ] Step 1: Write failing atomicity, freshness and concurrency tests

~~~
#[test]
fn one_slot_allows_one_concurrent_lease_and_no_orphan_reservation() { /* two barriers, one account, max_concurrency=1 */ }

#[test]
fn stale_or_json_cache_observation_fails_closed_without_user_hold() { /* no quota/lease rows after rejection */ }

#[test]
fn same_request_replays_one_lease_and_hash_conflict_is_rejected() { /* no second slot */ }

#[test]
fn timeout_unknown_survives_restart_and_is_not_ttl_released() { /* reopen store, recover, assert unknown */ }
~~~

Tests must use fixed now_ms values and fixtures for two providers, two regions, allowed/dedicated account constraints, and different value_scale values. They must assert the selection reason/observation id is auditable but secrets are absent.

- [ ] Step 2: Run the lease tests to verify failure

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test upstream_leases -- --nocapture

Expected: FAIL because the atomic scheduler methods and lease transitions are not implemented.

- [ ] Step 3: Implement preflight_reserve_with_lease

Use one BEGIN IMMEDIATE transaction to:

1. authenticate the existing Principal and scope;
2. resolve cost policy and idempotency hash;
3. replay an existing terminal request without creating another reservation/lease;
4. filter enabled accounts by provider, region, capabilities, allowed/dedicated constraints, account state, observation source/status/freshness, available_units >= predicted_units + safety_margin_units, and active held/active/unknown slot count below max_concurrency;
5. deterministically rank candidates according to the configured strategy and tie-break by account ref;
6. insert request, quota reservation, lease and audit metadata atomically.

If no candidate exists, return one stable ScheduleError category and commit no quota or lease row. Use a unique (request_id, resource_kind) constraint as the second idempotency guard.

- [ ] Step 4: Implement heartbeat, settlement and restart recovery

settle_upstream_lease must be conditional on state in held/active; Success commits/releases the user reservation through existing quota code and marks the lease succeeded; explicit unaccepted rejection releases both; accepted/unknown transport marks both request and lease unknown and sets reconcile_until_ms. Repeated settlement returns the stored terminal result. recover_expired_upstream_leases(now_ms) marks expired held/active rows unknown, never releases them, and writes an audit event.

- [ ] Step 5: Run tests and commit

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test upstream_leases -- --nocapture

Then run: cargo test --manifest-path src-core/Cargo.toml --offline

~~~powershell
git add src-core/src/upstream.rs src-core/src/store.rs src-core/src/quota.rs src-core/src/requests.rs src-core/src/lib.rs src-core/tests/upstream_leases.rs src-core/tests/quota.rs
git commit -m "feat: add atomic upstream lease scheduling"
~~~

### Task 3: Add observation ports and deterministic Mock/fixture adapters

**Files:**
- Modify: src-core/src/ports.rs
- Modify: src-core/src/models.rs
- Modify: src-core/src/lib.rs
- Create: src-tauri/src/api_server/upstream_observation.rs
- Modify: src-tauri/src/api_server/mod.rs
- Modify: src-tauri/src/commands/accounts.rs:848-1110
- Modify: src-tauri/src/commands/workbuddy/credits.rs:1-90
- Test: src-core/tests/observation_ports.rs
- Test: src-tauri/src/api_server/upstream_observation.rs (unit tests)

**Interfaces:**
- Consumes: Task 1 observation models, existing Trae entitlement parser, WorkBuddy credits parser/cache, Tauri vault and AppState.
- Produces: ObservationRequest, ObservationSnapshot, ObservationReader, MockObservationReader, TauriObservationReader::read_trae, TauriObservationReader::read_workbuddy, and CoreStore::record_observation_snapshot。

- [ ] Step 1: Write failing port contract tests

~~~
#[test]
fn mock_observer_returns_scaled_value_and_never_reads_network() { /* fixture only */ }

#[test]
fn failed_observation_keeps_previous_value_and_marks_latest_attempt_failed() { /* append-only */ }

#[test]
fn work_and_general_observations_cannot_be_cross_selected() { /* resource_kind exact match */ }
~~~

Use a test counter/fixture closure instead of an HTTP mock server; the counter must prove no AGENT_HOST or WorkBuddy URL is touched.

- [ ] Step 2: Run the focused tests to verify failure

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test observation_ports -- --nocapture

Expected: FAIL because the port types and adapter functions do not exist.

- [ ] Step 3: Add the port types and Mock implementation

Add the exact types from the spec and re-export them from aiwork_core. MockObservationReader takes an in-memory HashMap<(account_ref, resource_kind), ObservationSnapshot> and returns clones; it has no filesystem/network fallback. record_observation_snapshot validates scale, timestamps, source, summary size and appends a row without changing quota tables.

- [ ] Step 4: Expose narrow Tauri read-only adapter functions

Make the existing Trae CreditStats/entitlement calculation available through a pub(crate) read-only wrapper that accepts an account id and returns ObservationSnapshot; do not duplicate parsing. Add a WorkBuddy wrapper around the existing parser/cache output with an explicit stale result. The wrappers may update the legacy JSON/cache mirror for UI compatibility, but the Core observation write is the authoritative scheduler input. Never place JWT/token values in the snapshot.

- [ ] Step 5: Run tests and commit

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test observation_ports -- --nocapture

Then run: cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline api_server::upstream_observation

~~~powershell
git add src-core/src/ports.rs src-core/src/models.rs src-core/src/lib.rs src-core/tests/observation_ports.rs src-tauri/src/api_server/upstream_observation.rs src-tauri/src/api_server/mod.rs src-tauri/src/commands/accounts.rs src-tauri/src/commands/workbuddy/credits.rs
git commit -m "feat: add upstream observation ports"
~~~

### Task 4: Add scheduler mode, account synchronization and recovery startup

**Files:**
- Modify: src-tauri/src/api_server/gateway_settings.rs
- Modify: src-tauri/src/models.rs
- Modify: src-tauri/src/commands/api_server.rs
- Modify: src-tauri/src/api_server/core_bridge.rs
- Modify: src-tauri/src/api_server/mod.rs
- Create: src-tauri/src/api_server/scheduler.rs
- Test: src-tauri/src/api_server/gateway_settings.rs (unit tests)
- Test: src-tauri/src/api_server/scheduler.rs (unit tests)

**Interfaces:**
- Consumes: Task 1/2 CoreStore methods, Task 3 observation/adapter ports, existing api_pool.json, vault accounts, groups, cooldowns and WorkBuddy pool.
- Produces: SchedulerMode::{Off,Shadow,Enforce}, SchedulerRuntime, sync_upstream_accounts, scheduler_status, and CoreBridge::scheduler()。

- [ ] Step 1: Write failing mode and sync tests

~~~
#[test]
fn scheduler_mode_defaults_to_off_and_round_trips_unknown_as_error() { /* legacy config */ }

#[test]
fn sync_writes_opaque_account_refs_without_credentials_or_user_grants() { /* Trae + WB fixtures */ }

#[test]
fn shadow_records_dry_run_but_enforce_rejects_missing_scheduler() { /* no legacy fallback */ }
~~~

- [ ] Step 2: Run the focused tests to verify failure

Run:

~~~powershell
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline gateway_settings
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline scheduler
~~~

Expected: FAIL because the setting, runtime and sync path do not exist.

- [ ] Step 3: Implement settings and account sync

Add scheduler_mode to GatewaySettings with serde default off. At API startup, after Core opens/migrates and before route serving, sync enabled Trae/WB accounts into Core using stable opaque refs, provider, region, confirmed capabilities, max concurrency, enabled state and credential ref. Do not copy JWT/token into Core. Import old credit files only as json_cache stale observations. The sync operation is idempotent and does not write quota grants.

- [ ] Step 4: Implement shadow/enforce runtime and startup recovery

Create a scheduler runtime containing CoreStore, observation reader registry and executor registry. In shadow, expose dry-run candidate diagnostics and audit only; in enforce, require a Core scheduler and call recover_expired_upstream_leases before accepting traffic. If core_mode=enforce is paired with scheduler_mode=off|shadow, return stable configuration/request errors rather than legacy fallback.

- [ ] Step 5: Run tests and commit

Run:

~~~powershell
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline gateway_settings
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline scheduler
~~~

~~~powershell
git add src-tauri/src/api_server/gateway_settings.rs src-tauri/src/models.rs src-tauri/src/commands/api_server.rs src-tauri/src/api_server/core_bridge.rs src-tauri/src/api_server/mod.rs src-tauri/src/api_server/scheduler.rs
git commit -m "feat: add scheduler modes and account synchronization"
~~~

### Task 5: Wire the scheduler lease into non-stream Chat

**Files:**
- Create: src-tauri/src/api_server/core_executor.rs
- Modify: src-tauri/src/api_server/routes.rs
- Modify: src-tauri/src/api_server/core_bridge.rs
- Modify: src-tauri/src/api_server/payload.rs
- Modify: src-tauri/src/api_server/pool.rs
- Test: src-tauri/src/api_server/core_executor.rs (unit tests)
- Test: src-tauri/src/api_server/routes.rs (Core enforce tests)

**Interfaces:**
- Consumes: Task 2 atomic preflight/lease, Task 3 ports, Task 4 SchedulerRuntime, existing non-stream Chat protocol projection and legacy ApiPool path.
- Produces: CoreUpstreamExecutor::execute_nonstream_chat, CoreChatContext.lease, and run_phase2_mock_chat() returning request id, reservation id, lease id, account ref, terminal states and Mock call count。

- [ ] Step 1: Write failing route and Mock flow tests

~~~
#[tokio::test]
async fn enforce_chat_uses_core_lease_and_mock_executor_once() { /* no real HTTP */ }

#[tokio::test]
async fn enforce_chat_does_not_fallback_to_legacy_pool_when_scheduler_rejects() { /* stable 503 */ }

#[tokio::test]
async fn request_body_account_and_user_fields_cannot_override_lease() { /* Core principal wins */ }

#[tokio::test]
async fn unintegrated_stream_or_protocol_returns_scheduler_endpoint_not_enabled() { /* no budget bypass */ }
~~~

- [ ] Step 2: Run the focused route tests to verify failure

Run:

~~~powershell
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline core_executor
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline core_
~~~

Expected: FAIL because the lease-aware executor and enforce route are not wired.

- [ ] Step 3: Implement the lease-aware executor seam

Create CoreUpstreamExecutor that receives UpstreamLeaseGrant, resolves its provider and credential reference through the registered adapter, and returns UpstreamOutcome. The production adapter may call existing Trae/WB transport code only after Core has selected the account; it must not call ApiPool::pick_*. The test adapter is MockUpstreamExecutor and records lease id/account ref without network access.

- [ ] Step 4: Integrate non-stream OpenAI Chat with atomic settlement

In chat_completions, after authentication, scope and cost validation, call the scheduler-aware Core preflight. On replay return the stored terminal response. On lease rejection return a stable no_fresh_observation/no_upstream_capacity error with no user hold. Pass the lease to the executor; map success/rejected/transport unknown through one settlement helper that settles both user reservation and upstream lease. Keep legacy off/shadow behavior unchanged. Enforce stream and unintegrated endpoints must not enter the legacy pool.

- [ ] Step 5: Run the focused flow and commit

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test full_phase2 -- --nocapture

Then run:

~~~powershell
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline core_executor
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline core_
~~~

~~~powershell
git add src-tauri/src/api_server/core_executor.rs src-tauri/src/api_server/routes.rs src-tauri/src/api_server/core_bridge.rs src-tauri/src/api_server/payload.rs src-tauri/src/api_server/pool.rs
git commit -m "feat: route non-stream chat through upstream leases"
~~~

### Task 6: Persist error classification, recovery, audit and scheduler status

**Files:**
- Modify: src-core/src/upstream.rs
- Modify: src-core/src/store.rs
- Modify: src-tauri/src/api_server/scheduler.rs
- Modify: src-tauri/src/api_server/routes.rs
- Modify: src-tauri/src/api_server/api_logger.rs
- Modify: src-tauri/src/commands/api_server.rs
- Create: docs/credit-aware-scheduler-operations.md
- Modify: docs/core-foundation-operations.md
- Modify: AGENT.md
- Test: src-core/tests/recovery.rs
- Test: src-tauri/src/api_server/scheduler.rs (status/error tests)

**Interfaces:**
- Consumes: Tasks 1–5 state machine, mode setting, executor outcomes and existing ErrKind mapping.
- Produces: persisted account health transitions, scheduler_status diagnostics, structured lease/observation events and Phase 2 operating instructions。

- [ ] Step 1: Write failing recovery and redaction tests

~~~
#[test]
fn expired_active_lease_becomes_unknown_after_store_reopen() { /* no release */ }

#[test]
fn transport_and_credit_errors_update_account_state_without_logging_secrets() { /* redacted audit */ }

#[test]
fn scheduler_status_exposes_counts_but_not_credentials_or_other_users() { /* admin-only shape */ }
~~~

- [ ] Step 2: Run the focused tests to verify failure

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test recovery -- --nocapture

Expected: FAIL until recovery/status/redaction paths are implemented.

- [ ] Step 3: Implement persistent health and structured events

Map existing ErrKind to upstream_accounts.state, cooldown and consecutive error fields. Emit audit metadata with request/lease/account hash/provider/resource/observation/error category only. Add scheduler status counts for fresh/stale, active/unknown leases, slot saturation and reader failures; do not expose exact tokens, JWT, cookies, prompt or complete upstream body.

- [ ] Step 4: Write and validate the operations documentation

Document schema v6 backup, scheduler_mode rollout, read-only refresh, shadow parity, enforce prerequisites, error meanings, unknown handling and rollback to off. State explicitly that upstream observations are not user grants and that Phase 2 evidence uses Mock execution; update AGENT.md command/config contract without touching unrelated dirty docs.

- [ ] Step 5: Run tests and commit

Run: cargo test --manifest-path src-core/Cargo.toml --offline --test recovery -- --nocapture

~~~powershell
git add src-core/src/upstream.rs src-core/src/store.rs src-core/tests/recovery.rs src-tauri/src/api_server/scheduler.rs src-tauri/src/api_server/routes.rs src-tauri/src/api_server/api_logger.rs src-tauri/src/commands/api_server.rs docs/credit-aware-scheduler-operations.md docs/core-foundation-operations.md AGENT.md
git commit -m "feat: persist scheduler recovery and operations status"
~~~

### Task 7: Phase 2 integration smoke, compatibility regression and delivery verification

**Files:**
- Create: src-core/tests/full_phase2.rs
- Create: src-tauri/src/api_server/phase2_smoke.rs
- Modify: src-core/src/ports.rs
- Modify: src-tauri/src/api_server/mod.rs
- Modify: docs/credit-aware-scheduler-operations.md
- Test: existing Rust, frontend and Python suites

**Interfaces:**
- Consumes: Tasks 1–6 public Core and Tauri scheduler interfaces.
- Produces: run_phase2_mock_chat() and an auditable Phase 2 verification report; no real upstream result claim。

- [ ] Step 1: Write the fixed end-to-end Mock scenario

The scenario must create two accounts with different providers/regions, fresh observations and one effective slot; constrain the fixture so both requests target the same provider/region and only one selected account is eligible with max_concurrency=1; create an admin/user/key and a user grant; run two concurrent non-stream Chat requests; assert one success and one deterministic no_capacity result, successful lease settlement, same-key replay without a second Mock call, stale observation rejection, explicit rejection release, timeout unknown retention, store reopen recovery, audit redaction and no user/quota overdraw.

- [ ] Step 2: Run the scenario and all expected tests

Run:

~~~powershell
cargo test --manifest-path src-core/Cargo.toml --offline --test full_phase2 -- --nocapture
cargo test --manifest-path src-core/Cargo.toml --offline
cargo test --manifest-path src-tauri/Cargo.toml --target-dir src-tauri/target-fix --offline
npm test
python src-python/tests/test_wb_credits.py
python src-python/tests/test_auto_checkin.py
~~~

Do not run src-python/tests/test_api_server.py as Phase 2 evidence if it reaches a real upstream. Record exit codes and test counts.

- [ ] Step 3: Run static boundary checks

Run:

~~~powershell
git diff --check
git status --short
git diff --name-only 8c35f53..HEAD
~~~

Confirm the task commit range contains only the planned Core/Tauri/tests/docs files and no data/, credentials, target artifacts or unrelated user changes.

- [ ] Step 4: Commit the integration evidence

~~~powershell
git add src-core/tests/full_phase2.rs src-tauri/src/api_server/phase2_smoke.rs src-core/src/ports.rs src-tauri/src/api_server/mod.rs docs/credit-aware-scheduler-operations.md
git commit -m "test: verify credit-aware scheduler phase two"
~~~

- [ ] Step 5: Produce the review package and stop for independent review

Create a review package from the Phase 2 base 8c35f53 through the final commit using the current subagent-driven-development review-package script. The independent reviewer must check Core atomicity, no legacy fallback in enforce, stale policy, lease recovery, secret hygiene, user isolation and test evidence before Phase 2 is called complete.

## Plan Self-Review

- Spec coverage: Tasks 1–2 cover schema, atomic selection, concurrency, quota/lease settlement and migration; Task 3 covers observation ports and real read-only adapters; Task 4 covers modes, sync and startup recovery; Task 5 covers non-stream Chat routing; Task 6 covers persistence, errors, audit, docs and status; Task 7 covers end-to-end and compatibility evidence. Stream/media/cancel/process/deployment requirements remain explicitly assigned to later phases.
- Placeholder scan: no unfinished markers or undefined test-only handoff remains in the task steps.
- Type consistency: ObservationSnapshot/ObservationReader feed CoreStore::record_observation_snapshot; SchedulerLeaseRequest feeds preflight_reserve_with_lease; UpstreamLeaseGrant feeds CoreUpstreamExecutor; UpstreamOutcome feeds the single settlement helper. Later tasks consume only interfaces produced by earlier tasks.
- Review focus coverage: all five failure classes are pinned to Tasks 1–3, 5–7 and the final review package.

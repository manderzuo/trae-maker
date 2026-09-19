# AI Work Assistant Phase 3A Streaming Lease Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 OpenAI Chat、OpenAI Responses 和 Anthropic Messages 的流式请求接入 Core 权威 quota/upstream lease 生命周期，并在取消、断流、重启和未知结果时保守 fail-closed。

**Architecture:** 在现有 `CoreStore` v6 lease 之上增加 schema v7 的取消状态和 `Canceled` settlement；在 Tauri bridge 增加无凭据的 `LeaseStreamAdapter`/Mock 边界；路由只负责认证、协议投影、heartbeat 和单一 settlement helper。真实 Trae/WB 流式 adapter 复用现有 SSE/凭据代码，但只有显式 Core account binding 存在时才注册，未注册时保持 501，绝不进入旧 `ApiPool` 流式路径。

**Tech Stack:** Rust 2021, `aiwork-core`, rusqlite/SQLite WAL, Tauri/Axum, Tokio channels, existing Trae/WB SSE converters, Rust integration tests; all verification targets and logs on `D:\gpt`.

**Spec:** `docs/superpowers/specs/2026-09-19-aiwork-phase3-streaming-design.md`

## Global Constraints

- Core `Principal.user_id` is the only business owner; request-body identity and legacy API-Key ownership cannot override it.
- A new idempotency key creates at most one user reservation and one upstream lease; Replay never returns an executable grant.
- Unknown, timeout, disconnect, process exit, unsupported cancellation, and incomplete stream termination retain quota/lease for reconciliation.
- Confirmed cancellation and explicit unaccepted rejection release exactly once; repeated settlement does not repeat ledger, health, or audit effects.
- Off/Shadow legacy streaming behavior remains unchanged; Core Enforce never falls back to `ApiPool`.
- No raw key, JWT, cookie, prompt, complete upstream body, or resource token enters persisted state, logs, or test fixtures.
- Tests use Mock/fixture adapters only; no real upstream generation or billing claim is evidence for this phase.
- Every Cargo test clears/asserts absent `AIWORK_*`, sets `TEMP`/`TMP` under `D:\gpt`, uses `--offline --locked`, and writes target/log output under `D:\gpt`.

## Review Focus

1. A client disconnect during an active stream must produce `cancel_requested` plus `unknown` unless cancellation is explicitly confirmed; test in Task 3 with a closed sink and an adapter that cannot cancel.
2. A same-key concurrent/replayed stream must never dispatch twice or return a reusable grant; test in Task 1/3 with one CoreStore and a counted Mock adapter.
3. A heartbeat or cancellation request for another user or a terminal lease must have no state or audit side effect; test in Task 1 with owner and terminal cases.
4. Missing stream adapter/provider binding must return stable 501 before reservation/lease and must not call legacy pool code; test in Task 3/4 with a fail-fast pool sentinel.
5. Protocol-specific terminal/error frames and missing usage must not be treated as successful billable usage; test OpenAI/Responses/Anthropic contract cases in Task 3.

---

### Task 1: Core schema v7 cancellation and streaming settlement

**Files:**
- Modify: `src-core/src/models.rs` (`RequestState` and transition matrix)
- Modify: `src-core/src/schema.rs` (v7 request-state migration)
- Modify: `src-core/src/store.rs` (schema version/migration dispatch)
- Modify: `src-core/src/upstream.rs` (`LeaseOutcome::Canceled`)
- Modify: `src-core/src/requests.rs` (cancel request, canceled settlement, recovery transitions)
- Create: `src-core/tests/streaming.rs`

**Interfaces:**
- Consumes: existing v6 `requests`, quota reservation, `upstream_leases`, `heartbeat_upstream_lease`, `settle_upstream_lease_with_status`, and `Principal` owner validation.
- Produces: `RequestState::{CancelRequested,Canceled}`, `LeaseOutcome::Canceled`, `CoreStore::request_upstream_cancel(&Principal, &str, i64)`, and settlement/recovery behavior consumed by Tauri Tasks 2–4.

- [ ] **Step 1: Write the failing Core tests**

Add `src-core/tests/streaming.rs` tests with fixed `D:\gpt` fixtures:

~~~rust
#[test]
fn v7_preserves_existing_requests_and_allows_cancel_states() { /* migrate v6, assert rows and legal transitions */ }

#[test]
fn cancel_requires_owner_and_does_not_release_before_confirmation() { /* request cancel, assert CancelRequested + held/unknown reservation */ }

#[test]
fn confirmed_cancel_releases_once_and_replay_has_no_grant() { /* Canceled settlement twice, assert one ledger event */ }

#[test]
fn heartbeat_terminal_or_foreign_lease_has_no_side_effect() { /* owner/terminal checks */ }
~~~

- [ ] **Step 2: Run the Core tests and verify the expected red result**

Run:

~~~powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; Get-ChildItem Env:AIWORK_* -ErrorAction SilentlyContinue | Remove-Item -ErrorAction SilentlyContinue
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-phase3-task1-red --offline --locked --test streaming -- --nocapture 2>&1 | Tee-Object D:\gpt\aiwork-phase3-task1-red.log
~~~

Expected: FAIL because schema version 7, cancel states, `LeaseOutcome::Canceled`, and `request_upstream_cancel` do not yet exist.

- [ ] **Step 3: Add the v7 migration and typed state transitions**

Set `CURRENT_SCHEMA_VERSION` to `7`; rebuild the constrained `requests` and `idempotency_keys` tables in one transaction with `cancel_requested` and `canceled` in the state check; preserve every v6 row and update migration dispatch. Extend `RequestState::can_transition_to` exactly as the spec state machine describes. Add `LeaseOutcome::Canceled` and map it to request `Canceled`, lease `Failed`, and `Settlement::Release` only after owner validation. Implement `request_upstream_cancel` as an immediate transaction that validates the lease owner, accepts only `held`/`active` requests, records `cancel_requested` and one audit event, and does not release quota.

- [ ] **Step 4: Run the focused Core tests and verify green**

Run the same command with target `D:\gpt\aiwork-phase3-task1-green`; expected `4 passed; 0 failed`, with no real network and no `AIWORK_*` variables.

- [ ] **Step 5: Run the existing Core lease/recovery regression set**

~~~powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; Get-ChildItem Env:AIWORK_* -ErrorAction SilentlyContinue | Remove-Item -ErrorAction SilentlyContinue
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-phase3-task1-suite --offline --locked --tests -- --nocapture 2>&1 | Tee-Object D:\gpt\aiwork-phase3-task1-suite.log
~~~

Expected: all existing Core tests plus the four streaming tests pass; old v1–v6 migration evidence remains green.

- [ ] **Step 6: Commit Task 1**

~~~powershell
git add src-core/src/models.rs src-core/src/schema.rs src-core/src/store.rs src-core/src/upstream.rs src-core/src/requests.rs src-core/tests/streaming.rs
git commit -m "feat: add core streaming cancellation states"
~~~

### Task 2: Add the credential-free stream adapter boundary and Mock

**Files:**
- Modify: `src-tauri/src/api_server/core_executor.rs`
- Modify: `src-tauri/src/api_server/core_bridge.rs`
- Modify: `src-core/src/ports.rs` only if the shared request/event type must be exported from Core
- Test: `src-tauri/src/api_server/core_executor.rs` unit tests

**Interfaces:**
- Consumes: Task 1 `UpstreamLeaseGrant`, `LeaseOutcome`, owner-checked Core store methods.
- Produces: `StreamUsage`, `StreamEvent`, `CancelSupport`, `StreamTerminalOutcome`, `StreamSink`, `LeaseStreamAdapter`, `CoreUpstreamExecutor::with_stream_provider`, `CoreUpstreamExecutor::can_dispatch_stream`, and `CoreUpstreamExecutor::execute_stream`.

- [ ] **Step 1: Write the failing adapter-boundary tests**

Add tests that assert an exact account binding is required, an unbound account returns `account_binding_missing`, `StreamSink::emit(false)` is not success, and Mock terminal outcomes preserve `actual_units=None` when usage is absent.

- [ ] **Step 2: Run the focused adapter tests and verify red**

Run:

~~~powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; Get-ChildItem Env:AIWORK_* -ErrorAction SilentlyContinue | Remove-Item -ErrorAction SilentlyContinue
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-phase3-task2-red --offline --locked core_bridge::core_executor -- --nocapture 2>&1 | Tee-Object D:\gpt\aiwork-phase3-task2-red.log
~~~

Expected: FAIL because no stream types, registry, or execution method exists.

- [ ] **Step 3: Implement the minimal stream types and registry**

Add the exact types from the Phase 3A spec. Keep stream adapters separate from non-stream adapters so providers without a safe stream implementation cannot be treated as stream-capable. `execute_stream` must verify `account_ref` and `credentials_ref` before invoking the adapter and return `TransportUnknown { reason: "stream_adapter_unavailable" }` for missing registrations.

- [ ] **Step 4: Add a deterministic Mock adapter**

The Mock emits a fixed sequence of text/usage/terminal events, records lease id/account ref/request id, and supports configured `Success`, `Rejected`, `Canceled`, and `TransportUnknown` outcomes. It has no filesystem, credentials, pool, or network fallback.

- [ ] **Step 5: Run focused adapter tests and the existing Core bridge tests**

Expected: all new binding/sink tests pass and the existing Core executor/bridge tests remain green. Write logs and target only under `D:\gpt`.

- [ ] **Step 6: Commit Task 2**

~~~powershell
git add src-tauri/src/api_server/core_executor.rs src-tauri/src/api_server/core_bridge.rs src-core/src/ports.rs
git commit -m "feat: add credential-free streaming adapter boundary"
~~~

### Task 3: Wire Core Enforce streaming and protocol contract tests

**Files:**
- Modify: `src-tauri/src/api_server/routes.rs` (Core enforce stream branch and shared context)
- Modify: `src-tauri/src/api_server/sse.rs` or create `src-tauri/src/api_server/core_stream.rs` for normalized event projection
- Modify: `src-tauri/src/api_server/mod.rs` if a new focused module is created
- Create: `src-tauri/src/api_server/core_stream.rs` when projection/heartbeat code exceeds route responsibility
- Test: `src-tauri/src/api_server/core_stream.rs` and route tests in `routes.rs`

**Interfaces:**
- Consumes: Task 1 Core preflight/cancel/settlement and Task 2 `CoreUpstreamExecutor::execute_stream`.
- Produces: `core_stream_chat(...) -> Response`, one settlement path for all terminal outcomes, protocol encoders for OpenAI Chat/Responses/Anthropic, and a heartbeat/cancel task that owns no credentials.

- [ ] **Step 1: Write failing route/contract tests**

Add tests for:

~~~rust
#[tokio::test]
async fn core_stream_openai_mock_emits_done_and_settles_success() { /* inspect SSE chunks and Core states */ }

#[tokio::test]
async fn core_stream_anthropic_and_responses_emit_protocol_terminal_events() { /* exact event ordering */ }

#[tokio::test]
async fn closed_client_channel_requests_cancel_and_unknown_without_release() { /* Mock cancel unsupported */ }

#[tokio::test]
async fn core_stream_without_registered_adapter_returns_501_before_lease() { /* no legacy pool call */ }
~~~

- [ ] **Step 2: Run the route tests and verify red**

Run the route/core test filter with a D-drive target/log; expected failure is missing `core_stream_chat`/protocol projection and the current stable 501 branch.

- [ ] **Step 3: Implement the Core enforce stream response**

Refactor the existing non-stream Core preflight context only as needed. For `stream=true`, require idempotency, authenticate/scope-check, obtain explicit executor bindings, and call the same atomic preflight. Return replay metadata without executing. For a new lease, create an Axum body stream backed by a bounded Tokio channel, run the adapter in `spawn_blocking`, and settle exactly once after the terminal outcome.

- [ ] **Step 4: Implement heartbeat and disconnect handling**

Heartbeat at a configured interval shorter than the lease TTL, using the Core Principal and lease id. A failed heartbeat or closed output channel sets cancellation intent; the adapter may return `Canceled` only when confirmed. Otherwise call `request_upstream_cancel` and settle `TransportUnknown`. Do not release the reservation in the disconnect path.

- [ ] **Step 5: Implement protocol projection and error envelopes**

Map normalized Mock events into OpenAI Chat SSE (`data` plus `[DONE]`), Responses terminal events, and Anthropic message/content-block/message-stop events. Never infer billable actual units from missing usage. Ensure error categories pass through the existing allowlist and no raw event body is logged.

- [ ] **Step 6: Run the focused stream contract tests and existing Core route tests**

Expected: all four new route tests pass; existing non-stream replay, quota, identity, fail-closed, and legacy Off/Shadow route tests remain green. No real upstream is called.

- [ ] **Step 7: Commit Task 3**

~~~powershell
git add src-tauri/src/api_server/routes.rs src-tauri/src/api_server/core_stream.rs src-tauri/src/api_server/sse.rs src-tauri/src/api_server/mod.rs
git commit -m "feat: route core enforced streaming leases"
~~~

### Task 4: Add explicit legacy Trae/WB stream adapters and startup registration

**Files:**
- Modify: `src-tauri/src/api_server/core_executor.rs` (provider stream adapters)
- Modify: `src-tauri/src/api_server/scheduler.rs` (stream executor builder)
- Modify: `src-tauri/src/commands/api_server.rs` (Enforce registration/readiness)
- Modify: `src-tauri/src/api_server/routes.rs` only for adapter-specific protocol projection hooks
- Test: adapter unit tests and scheduler startup binding tests

**Interfaces:**
- Consumes: Task 2/3 stream contract, existing `payload`, `wb_payload`, `sse`, `wb_sse`, `make_upstream_request`, `make_wb_request`, and the committed explicit legacy account map.
- Produces: `build_legacy_stream_executor(...)` with exact account bindings; no wildcard provider registration; real adapter errors mapped to `StreamTerminalOutcome` conservatively.

- [ ] **Step 1: Write failing exact-account adapter tests**

Use injected transport fakes, not network. Assert Trae and WorkBuddy receive only the Core-selected legacy UID; missing mapping, 502/timeout, malformed SSE, and client disconnect become unknown; explicit unaccepted HTTP rejection releases; no alternate pool account is picked.

- [ ] **Step 2: Run the adapter tests and verify red**

Run the focused Tauri adapter/scheduler filters with D-drive target/log; expected failure is the missing stream adapter implementation/registration.

- [ ] **Step 3: Implement provider adapters by reusing existing transport boundaries**

Resolve only the mapped UID with `ApiPool::pick_by_uid`; never call `pick_*` or rotate after Core has selected the account. Convert provider SSE lines into the normalized stream events; map status/transport/parse outcomes through the existing allowlist. Preserve `actual_units=None` unless a verified adapter-specific policy supplies a safe amount.

- [ ] **Step 4: Register only explicit bindings at Enforce startup**

Extend the existing scheduler builder so each synced account registers `(account_ref, provider, credentials_ref)` for stream capability. If any required stream binding is missing, startup or the request returns stable `scheduler_endpoint_not_enabled`; do not expose a partially wildcarded stream endpoint.

- [ ] **Step 5: Run adapter, scheduler, core and fail-closed regressions**

Expected: new injected transport tests pass; existing 24 scheduler and 62 Core focused tests (or their current equivalent after schema changes) remain green. No test reaches a real upstream.

- [ ] **Step 6: Commit Task 4**

~~~powershell
git add src-tauri/src/api_server/core_executor.rs src-tauri/src/api_server/scheduler.rs src-tauri/src/commands/api_server.rs src-tauri/src/api_server/routes.rs
git commit -m "feat: bind legacy provider streaming adapters explicitly"
~~~

### Task 5: Phase 3A integration evidence and operations documentation

**Files:**
- Create: `src-core/tests/full_phase3_streaming.rs`
- Create or modify: `src-tauri/src/api_server/phase3_streaming_smoke.rs`
- Modify: `docs/credit-aware-scheduler-operations.md` or the existing Phase 3 operations document, without touching unrelated user-dirty docs
- Modify: `AGENT.md` only for the new D-drive test commands and Core stream feature gate

**Interfaces:**
- Consumes: Tasks 1–4 public state, stream adapter, route, and startup contracts.
- Produces: Mock-only end-to-end evidence for success/replay/rejection/cancel/unknown/reopen recovery and an auditable rollout/rollback note.

- [ ] **Step 1: Write the fixed end-to-end Mock scenario**

Cover one-user quota reservation, one selected account, bounded stream events, same-key no-duplicate execution, explicit rejection release, confirmed cancellation release, unsupported cancellation unknown retention, heartbeat extension, restart recovery, audit redaction, and missing adapter 501.

- [ ] **Step 2: Run the scenario and all required focused suites**

Run Core streaming/full tests, Tauri stream/core/scheduler filters, existing Core regression tests, frontend tests, and only the allowed pure-function Python tests. Clear/assert `AIWORK_*`; keep all targets/logs in `D:\gpt`; do not run the real-upstream Python API test.

- [ ] **Step 3: Run static boundary checks**

~~~powershell
git diff --check
git status --short
git diff --name-only dc266bf..HEAD
~~~

Confirm no `data/`, credentials, `target-fix`, generated lockfile, or unrelated user-dirty file entered the Phase 3A commit range.

- [ ] **Step 4: Commit integration evidence**

~~~powershell
git add src-core/tests/full_phase3_streaming.rs src-tauri/src/api_server/phase3_streaming_smoke.rs docs/credit-aware-scheduler-operations.md AGENT.md
git commit -m "test: verify phase three streaming lifecycle"
~~~

- [ ] **Step 5: Final review package**

Create a review package from the Phase 3A base through the final commit. The reviewer must check cancellation/unknown semantics, Core identity and idempotency, protocol terminal events, exact provider binding, no legacy fallback, secret redaction, D-drive evidence, and preservation of the existing dirty worktree. Critical/Important findings require one RED→GREEN fix pass before Phase 3A is accepted.

## Completion Contract

Phase 3A is complete only when every task has a passing final test command, the schema migration and route contract are reviewed, the integration smoke is green, the real provider path is either injected-tested and explicitly bound or remains fail-closed, and the final review package contains no unresolved Critical/Important finding. Phase 3B素材归属、Phase 3C视频 jobs/attempts and Phase 4 management/deployment remain separate goals.

# Video Actual Credit Settlement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop false fixed-1 video charges and make Core settle a video request only once a task-level, verifiable upstream credit receipt is available; otherwise keep the request safely held and visible for reconciliation.

**Architecture:** Core owns admission, the persistent video billing gate, user quota reservations, settlement state, and audit facts. AI Work owns the native Seedance call and exposes only a sanitized task-level billing receipt candidate; Core independently validates the candidate before committing user credits. A background reconciler polls accepted tasks so settlement does not depend on the user repeatedly opening the status endpoint.

**Tech Stack:** Rust, Axum, SQLite via `rusqlite`, Serde/JSON, existing Core quota/request state machines, existing AI Work native Seedance SSE adapter, static HTML admin console, Rust integration tests, PowerShell build scripts.

**Spec:** `docs/superpowers/specs/2026-09-22-video-actual-credit-settlement-design.md`

## Global Constraints

- Default video billing mode is persistent `paused`; it blocks new direct video and `seedance` Chat submissions before quota reservation and upstream forwarding.
- A one-time diagnostic allowance may be consumed only by the already existing ordinary Key selected by the administrator; it is atomic, request-hash-bound, non-replayable, and never becomes a permanent bypass.
- A 202/task-created response creates a held reservation only; it never commits a fixed 1 credit.
- Only a receipt proving task identity, upstream credit units, non-negative supported precision, and repeatable value may produce `Commit { actual_amount }`.
- Aggregate AI Work balance deltas, token counts, video duration, CNY/USD costs, and fixed estimates are never used as actual user credit consumption.
- Accepted-but-unpriced, disconnected, malformed, conflicting, or over-limit tasks remain `unknown`/`reconcile_required`; they are not silently released, committed, or retried.
- Existing integer `credits` behavior is not migrated to fixed-point units until a real task receipt and a safe upper bound are verified; no partial unit migration is allowed.
- Tests, Cargo target data, backups, and temporary files go under `D:\gpt`; do not add repeated read/write tests to C: and do not delete unrelated user files.
- Never print or persist plaintext API keys, JWTs, cookies, full prompts, raw SSE payloads, or upstream account identifiers in tests, logs, admin JSON, or the UI.
- Preserve all unrelated dirty worktree changes; stage and commit only files belonging to the current task.

## Review Focus

- A task accepted with HTTP 202 but not completed must show `held`, not `settled`; Task 3 tests the reservation and response path.
- A task that completed without a verifiable receipt must remain `reconcile_required` with its hold intact; Tasks 2 and 3 test this separately and together.
- Repeated status polling, concurrent polling, process restart, and replay must create at most one final ledger settlement; Task 3 owns the state-machine tests.
- A diagnostic allowance must reject the wrong Key, wrong body hash, and second use; Task 1 owns the atomic claim tests.
- A malformed, negative, fractional-out-of-range, conflicting, or over-reservation receipt must never reduce a user balance; Tasks 2 and 3 own receipt-validation tests.

---

### Task 1: Persistent Core video billing control and one-time diagnostic admission

**Files:**
- Create: `src-core/src/video_billing.rs`
- Modify: `src-core/src/lib.rs`
- Modify: `src-core/src/models.rs`
- Modify: `src-core/src/store.rs: schema migration dispatch and CoreStore methods`
- Test: `src-core/tests/video_billing.rs`
- Test: `src-core/tests/schema_bootstrap.rs`

**Interfaces:**
- Consumes: existing `CoreStore`, `Principal`, `request_hash` convention, SQLite schema versioning, and admin audit insertion helpers.
- Produces: `VideoBillingMode::{Paused, DiagnosticOnce, Active}`, `VideoBillingControl`, `VideoDiagnosticClaim`, `CoreStore::video_billing_control`, `CoreStore::set_video_billing_control`, and `CoreStore::claim_video_diagnostic`.

- [ ] **Step 1: Write failing persistence and claim tests**

  Add `src-core/tests/video_billing.rs` with these cases:

  ```rust
  #[test]
  fn new_store_defaults_to_paused_without_a_claim() {
      let store = test_store("video-billing-default");
      let control = store.video_billing_control().unwrap();
      assert_eq!(control.mode, VideoBillingMode::Paused);
      assert!(control.reason.contains("未取得"));
      assert!(control.diagnostic_key_id.is_none());
  }

  #[test]
  fn diagnostic_claim_is_bound_to_key_and_request_hash_and_is_one_shot() {
      let store = test_store("video-billing-claim");
      let request_hash = hex::encode(request_hash("videos", "seedance", &json!({"prompt":"x"})));
      let other_hash = hex::encode(request_hash("videos", "seedance", &json!({"prompt":"y"})));
      store.set_video_billing_control(VideoBillingControlInput::diagnostic(
          "key-week", &request_hash, "真实验收",
      )).unwrap();
      assert!(store.claim_video_diagnostic("key-week", &other_hash).unwrap().is_none());
      assert!(store.claim_video_diagnostic("key-other", &request_hash).unwrap().is_none());
      let first = store.claim_video_diagnostic("key-week", &request_hash).unwrap();
      assert!(first.is_some());
      assert!(store.claim_video_diagnostic("key-week", &request_hash).unwrap().is_none());
  }

  #[test]
  fn migration_preserves_existing_quota_rows_and_sets_versioned_gate() {
      let store = test_store("video-billing-migration");
      let before = store.schema_version().unwrap();
      store.migrate().unwrap();
      assert!(store.schema_version().unwrap() >= before);
      assert!(store.video_billing_control().unwrap().mode == VideoBillingMode::Paused);
      assert_eq!(count_table(&store, "quota_ledger"), 0);
  }
  ```

- [ ] **Step 2: Run the focused tests and confirm they fail for the missing API/schema**

  Run from the repository root with the target directory on D:

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p aiwork-core --test video_billing -- --nocapture
  ```

  Expected: compile failure because `video_billing` types and `CoreStore` methods do not exist yet. No production database or public process is touched.

- [ ] **Step 3: Add the versioned gate schema and typed Core API**

  In `src-core/src/video_billing.rs`, define the serializable control and input types. Keep the state names stable:

  ```rust
  pub enum VideoBillingMode { Paused, DiagnosticOnce, Active }
  pub struct VideoBillingControl {
      pub mode: VideoBillingMode,
      pub reason: String,
      pub diagnostic_key_id: Option<String>,
      pub diagnostic_request_hash: Option<String>,
      pub diagnostic_claimed_at_ms: Option<i64>,
      pub updated_at_ms: i64,
  }
  pub struct VideoBillingControlInput {
      pub mode: VideoBillingMode,
      pub reason: String,
      pub diagnostic_key_id: Option<String>,
      pub diagnostic_request_hash: Option<String>,
  }
  pub struct VideoDiagnosticClaim { pub claimed_at_ms: i64 }
  ```

  Add a singleton `video_billing_control` table and a unique `video_diagnostic_claims` table in a new schema migration after the current schema version. The claim table must use a unique `(key_id, request_hash)` pair and a single active claim ID. `claim_video_diagnostic` must run in `BEGIN IMMEDIATE`, verify mode and exact values, insert the claim, set the control to `Paused`, and return `Some` only for the first successful claim. Store only the internal Key ID and SHA-256 request hash.

  Export the module in `src-core/src/lib.rs`, re-export the public types, and add the migration to `CoreStore::migrate` with an idempotent default row. Do not change quota amounts or multiply existing integer balances in this task.

- [ ] **Step 4: Run Core tests and schema tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p aiwork-core --test video_billing --test schema_bootstrap -- --nocapture
  ```

  Expected: PASS; a fresh store reports paused, claims are one-shot, and the migration does not import or alter quota rows.

- [ ] **Step 5: Commit the isolated Core gate change**

  ```powershell
  git add src-core/src/video_billing.rs src-core/src/lib.rs src-core/src/models.rs src-core/src/store.rs src-core/tests/video_billing.rs src-core/tests/schema_bootstrap.rs
  git commit -m "feat: add persistent video billing gate"
  ```

### Task 2: AI Work task-level billing receipt candidate

**Files:**
- Modify: `src-tauri/src/api_server/video.rs: VideoTask, PersistedVideoTask, SSE result parsing, task response serialization`
- Test: `src-tauri/src/api_server/video.rs: parser tests`
- Test: `src-tauri/src/api_server/video.rs: parser and persistence unit tests`

**Interfaces:**
- Consumes: existing native Seedance SSE `result`/`output`/`done` events and the upstream task ID.
- Produces: sanitized `billing` data in the AI Work video task response, with `status` exactly `unverified`, `verified`, or `absent`; never includes account credentials or raw upstream data.

- [ ] **Step 1: Add red tests for candidate extraction and safety**

  Extend the existing video parser tests with these assertions:

  ```rust
  #[test]
  fn billing_candidate_requires_task_reference_and_credit_unit() {
      let candidate = extract_billing_candidate(
          r#"{"task_id":"video-1","usage":{"credits":12.5,"unit":"credits"}}"#,
          "video-1",
      ).unwrap();
      assert_eq!(candidate.status, BillingStatus::Verified);
      assert_eq!(candidate.actual_credits, Some("12.500000".into()));
  }

  #[test]
  fn balance_delta_duration_token_cost_and_unrelated_numbers_are_not_billing() {
      for data in [
          r#"{"video_duration":5,"credits":12.5}"#,
          r#"{"usage":{"total_tokens":999,"cost":"12.50","currency":"CNY"}}"#,
          r#"{"balance_before":100,"balance_after":80}"#,
      ] {
          assert_eq!(extract_billing_candidate(data, "video-1").unwrap().status, BillingStatus::Unverified);
      }
  }

  #[test]
  fn invalid_or_negative_receipts_are_exposed_as_unverified_without_failing_the_video_task() {
      let candidate = extract_billing_candidate(
          r#"{"task_id":"video-1","usage":{"credits":-1,"unit":"credits"}}"#,
          "video-1",
      ).unwrap();
      assert_eq!(candidate.status, BillingStatus::Unverified);
      assert!(candidate.actual_credits.is_none());
  }
  ```

  The first test is deliberately red until the actual upstream field contract is confirmed by the controlled diagnostic response. If the real response uses another verified field name, add that exact field to the allowlist and keep the negative cases red/green as written; do not treat arbitrary `cost`, `balance`, or `duration` values as credits.

- [ ] **Step 2: Run only the parser tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p aiwork-assistant api_server::video::tests -- --nocapture
  ```

  Expected before implementation: failure in the new receipt tests while the existing video tests remain runnable.

- [ ] **Step 3: Add an explicit sanitized receipt type and parser**

  Add a persisted `VideoBillingReceipt` to `VideoTask` and `PersistedVideoTask`:

  ```rust
  #[derive(Clone, Serialize, Deserialize)]
  pub struct VideoBillingReceipt {
      pub status: BillingStatus,
      pub actual_credits: Option<String>,
      pub unit: Option<String>,
      pub source: Option<String>,
      pub task_ref: Option<String>,
      pub observed_at_ms: u64,
  }
  ```

  Parse only an allowlisted task reference plus an explicit `credits` unit and decimal credit amount. Canonicalize to a fixed decimal string without converting through binary floating point. Reject negative, NaN-like, exponent, more-than-six-decimal, or non-finite values as `unverified`. Use `source="upstream_task_receipt"`; do not serialize the raw event. Preserve `absent` when no candidate exists. Update the SSE `result`/`output`/`done` paths and restart persistence. Do not mark the receipt verified merely because the SSE event or HTTP request succeeded.

- [ ] **Step 4: Run the parser and restart persistence tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p aiwork-assistant api_server::video::tests -- --nocapture
  cargo test -p ai-work-assistant api_server::video::tests -- --nocapture
  ```

  Expected: PASS for known-good fixture, rejection of unrelated/invalid values, and persistence of the sanitized status only.

- [ ] **Step 5: Commit the receipt contract change**

  ```powershell
  git add src-tauri/src/api_server/video.rs
  git commit -m "feat: expose sanitized video billing receipt"
  ```

### Task 3: Core held-to-final video settlement and background reconciliation

**Files:**
- Create: `starlink-dimension-router/src/video_billing.rs`
- Create: `starlink-dimension-router/src/video_reconciler.rs`
- Modify: `starlink-dimension-router/src/lib.rs`
- Modify: `starlink-dimension-router/src/state.rs`
- Modify: `starlink-dimension-router/src/user_routes.rs: admission, task query, seedance Chat path, settlement helper`
- Modify: `starlink-dimension-router/src/server.rs: reconciler startup and shared route state`
- Test: `starlink-dimension-router/tests/video_billing.rs` (create)
- Test: `starlink-dimension-router/tests/assets_api.rs: preserve existing video replay assertions`

**Interfaces:**
- Consumes: `CoreStore::video_billing_control`, existing `PreflightReserveResult`, bridge `GET /v1/videos/{id}`, and AI Work `billing` receipt.
- Produces: `VideoBillingDecision::{Held, Settled, Released, ReconcileRequired}`, `settle_video_job`, persisted `UserVideoJob.reservation_id`, and a 15-second reconciler that is idempotent and fail-closed.

- [ ] **Step 1: Write failing route/state tests with a fake bridge**

  Add a fake bridge fixture that returns controlled JSON without contacting AI Work. Pin these behaviors:

  ```rust
  #[tokio::test]
  async fn accepted_video_keeps_one_reservation_held() {
      let response = post_video_generation(&fixture(), json!({"model":"seedance","prompt":"x"})).await;
      assert_eq!(response.status(), StatusCode::ACCEPTED);
      assert_eq!(quota(&fixture(), "u", "credits").held, 1);
      assert_eq!(quota(&fixture(), "u", "credits").settled, 0);
  }

  #[tokio::test]
  async fn verified_receipt_commits_actual_credits_once() {
      let task = submit_with_fake_bridge(json!({"status":"queued"})).await;
      fake_bridge_set_status(&task, json!({
          "task":{"id":task,"status":"completed",
          "billing":{"status":"verified","actual_credits":"1","unit":"credits","task_ref":task}}
      }));
      poll_task(&task).await;
      poll_task(&task).await;
      assert_eq!(quota(&fixture(), "u", "credits").held, 0);
      assert_eq!(quota(&fixture(), "u", "credits").settled, 1);
      assert_eq!(ledger_entries_for_task(&task), 1);
  }

  #[tokio::test]
  async fn completed_without_verified_receipt_is_reconciliation_required() {
      let task = submit_with_fake_bridge(json!({"status":"queued"})).await;
      fake_bridge_set_status(&task, json!({"task":{"id":task,"status":"completed"}}));
      poll_task(&task).await;
      assert_eq!(job(&task).status, "reconcile_required");
      assert_eq!(quota(&fixture(), "u", "credits").held, 1);
      assert_eq!(quota(&fixture(), "u", "credits").settled, 0);
  }

  #[tokio::test]
  async fn fractional_receipt_is_stored_for_reconciliation_before_fixed_point_migration() {
      let task = submit_with_fake_bridge(json!({"status":"queued"})).await;
      fake_bridge_set_status(&task, json!({"task":{"id":task,"status":"completed",
          "billing":{"status":"verified","actual_credits":"12.500000","unit":"credits","task_ref":task}}}));
      poll_task(&task).await;
      assert_eq!(job(&task).status, "reconcile_required");
      assert_eq!(job(&task).actual_credits.as_deref(), Some("12.500000"));
      assert_eq!(quota(&fixture(), "u", "credits").settled, 0);
  }
  ```

  Add cases for confirmed pre-accept rejection releasing the hold, accepted upstream failure entering reconciliation, a receipt greater than the held upper bound, wrong `task_ref`, wrong unit, duplicate/concurrent polling, and diagnostic mode rejection before Core reservation for a non-claimed Key.

- [ ] **Step 2: Run the new router tests and confirm the old immediate-commit behavior fails the assertions**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p starlink-dimension-router --test video_billing -- --nocapture
  ```

  Expected before implementation: failure showing the current 202 path has `settled=1` and no held reservation after submission.

- [ ] **Step 3: Add admission gate and persist the reservation on `UserVideoJob`**

  In `starlink-dimension-router/src/video_billing.rs`, implement `admit_video_request` so it hashes the canonical request body using the existing Core request hash convention, reads the control, and returns:

  ```rust
  pub enum VideoAdmission { Paused, DiagnosticClaimed, Active }
  pub fn admit_video_request(
      store: &CoreStore,
      principal: &Principal,
      model: &str,
      body: &Value,
  ) -> Result<VideoAdmission, VideoBillingError>
  ```

  `Paused` returns a stable `503 video_billing_paused` response before `preflight`. `DiagnosticClaimed` is obtained only through the atomic Core claim and immediately changes the persisted control back to `Paused`. Add `reservation_id`, `billing_state`, `actual_credits`, and `last_reconciled_at_ms` to `UserVideoJob` with serde defaults for old job files. Use a named video reservation upper bound supplied by the configured validated cost policy; until that policy is explicitly verified, diagnostic mode may use a hold only for the one test and ordinary video remains paused.

- [ ] **Step 4: Remove the immediate video commit and add idempotent settlement**

  In `user_routes.rs`, both `video_generations` and the `seedance` branch of `chat_completions` must call the gate before `preflight`, keep the returned reservation ID in the job, and return 202 without `Settlement::Commit`. Add:

  ```rust
  fn settle_video_job(
      state: &StarlinkRouterState,
      job_id: &str,
      upstream: &Value,
  ) -> Result<VideoBillingDecision, CoreError>
  ```

  The helper must validate the task ID, status, receipt status, exact decimal scale, non-negative amount, and `actual_amount <= reserved_amount` before calling `CoreStore::settle_request`. Use the existing idempotent settlement replay behavior; never issue a second ledger event when the reservation is already final. If the bridge returns an accepted task with no receipt, store `reconcile_required` and leave the reservation held. `video_content` must never settle by itself.

- [ ] **Step 5: Add the background reconciler without blocking the async runtime**

  In `video_reconciler.rs`, start one task when the router is built. Use the named constant `VIDEO_RECONCILE_INTERVAL = Duration::from_secs(15)`. Every interval, take a short snapshot of nonterminal/reconcile jobs, call the bridge status endpoint with the job request ID from `spawn_blocking`, then invoke `settle_video_job`. On bridge timeout or process restart, retain `reconcile_required`; do not release, retry generation, or select a new upstream job. Stop the loop cleanly when the process is dropped in tests. Ensure a task query and the loop can race safely because Core settlement is transactional and replay-safe.

- [ ] **Step 6: Run focused route and full router tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p starlink-dimension-router --test video_billing -- --nocapture
  cargo test -p starlink-dimension-router --test assets_api -- --nocapture
  cargo test -p starlink-dimension-router --lib -- --nocapture
  ```

  Expected: PASS with no real network calls; 202 holds, verified receipt commits once, unknown stays held, explicit pre-accept rejection releases, and restart/replay does not double settle.

- [ ] **Step 7: Commit the Core async settlement change**

  ```powershell
  git add starlink-dimension-router/src/lib.rs starlink-dimension-router/src/state.rs starlink-dimension-router/src/video_billing.rs starlink-dimension-router/src/video_reconciler.rs starlink-dimension-router/src/user_routes.rs starlink-dimension-router/src/server.rs starlink-dimension-router/tests/video_billing.rs starlink-dimension-router/tests/assets_api.rs
  git commit -m "fix: hold video quota until verified settlement"
  ```

### Task 4: Admin controls, reconciliation visibility, and historical billing labels

**Files:**
- Modify: `starlink-dimension-router/src/admin_routes.rs`
- Modify: `starlink-dimension-router/static/index.html`
- Modify: `src-core/src/admin_summary.rs` for additive summary projections
- Modify: `src-core/src/models.rs` for additive summary fields
- Test: `starlink-dimension-router/tests/admin_api.rs` (create)
- Test: `src-core/tests/admin_projections.rs`

**Interfaces:**
- Consumes: the persistent Core billing control, job billing states, quota ledger summaries, and existing admin session middleware.
- Produces: authenticated `GET /admin/v1/video-billing`, `PUT /admin/v1/video-billing`, and `POST /admin/v1/video-billing/diagnostic` endpoints; additive summary fields for held, verified settled, and unverified legacy amounts.

- [ ] **Step 1: Write failing admin API tests**

  Pin these responses without exposing secrets:

  ```rust
  #[tokio::test]
  async fn admin_can_read_paused_state_and_unverified_counts() {
      let response = admin_get("/admin/v1/video-billing").await;
      assert_eq!(response.status(), StatusCode::OK);
      let body = json_body(response).await;
      assert_eq!(body["mode"], "paused");
      assert!(body.get("diagnostic_key_id").is_none() || body["diagnostic_key_id"].is_null());
      assert!(body.get("plaintext").is_none());
  }

  #[tokio::test]
  async fn diagnostic_endpoint_requires_key_id_hash_and_consumes_once() {
      let first = admin_put_diagnostic("key-week", "sha256:request", "验收").await;
      assert_eq!(first.status(), StatusCode::OK);
      let second = admin_put_diagnostic("key-week", "sha256:request", "验收").await;
      assert_eq!(second.status(), StatusCode::CONFLICT);
  }
  ```

- [ ] **Step 2: Implement authenticated controls and additive projection**

  Use the existing admin session/extension authentication. The GET response may include mode, reason, claim status, and counts of `held`, `reconcile_required`, `verified`, and `legacy_unverified`; it must not include Key plaintext, user Key prefix, upstream account details, or raw receipt data. The diagnostic endpoint accepts only `{key_id, request_hash, reason}` and delegates to the atomic Core claim. Add a historical label in the admin projection for pre-fix video commits, without changing their amount or silently writing a correction.

- [ ] **Step 3: Add settings UI and localized operator messages**

  Add a compact “视频计费安全状态” panel under the existing settings area. Show paused/diagnostic/active, held count,待对账 count, and legacy unverified count. Provide buttons for “保持暂停” and “登记一次性验收” that require an internal Key ID and request hash, and show the stable error text returned by Core. Never render plaintext Keys or upstream balances in this panel. Keep the homepage overview additive and keep the existing AI Work bridge settings in the settings section.

- [ ] **Step 4: Run admin and projection tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p starlink-dimension-router --test admin_api -- --nocapture
  cargo test -p aiwork-core --test admin_projections -- --nocapture
  ```

  Expected: PASS; unauthenticated requests are rejected, the one-shot claim is not replayable, and admin responses contain only sanitized billing state.

- [ ] **Step 5: Commit the operator visibility change**

  ```powershell
  git add starlink-dimension-router/src/admin_routes.rs starlink-dimension-router/static/index.html src-core/src/admin_summary.rs src-core/src/models.rs starlink-dimension-router/tests/admin_api.rs src-core/tests/admin_projections.rs
  git commit -m "feat: expose video billing reconciliation state"
  ```

### Task 5: Local fail-closed verification and public deployment preparation

**Files:**
- Modify: `scripts/test-starlink-public-deployment.mjs: add non-paid gate/status assertions`
- Create: `scripts/test-video-billing-state.mjs`
- Modify: `docs/core-foundation-operations.md`
- Modify: `docs/credit-aware-scheduler-operations.md`

**Interfaces:**
- Consumes: release build, local fake bridge, admin API, and `/healthz`.
- Produces: repeatable local verification that does not submit a real upstream video and a deployment checklist that keeps public video paused until receipt evidence is verified.

- [ ] **Step 1: Add a local fake-bridge integration harness**

  Implement `scripts/test-video-billing-state.mjs` using only a local test port and JSON fixtures. It must exercise: paused direct video, paused `seedance` Chat, diagnostic claim, 202 held state, verified receipt, unverified completion, explicit pre-accept rejection, duplicate status polling, and restart restore. It must fail if the fake bridge receives a request while the gate is paused.

- [ ] **Step 2: Run local verification with all writable paths on D:**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  pwsh -File .\scripts\build-starlink-router.ps1 -OutputRoot 'D:\gpt\starlink-core-video-status-release-next'
  node .\scripts\test-video-billing-state.mjs --data-dir 'D:\gpt\starlink-video-billing-test'
  node .\scripts\test-starlink-public-deployment.mjs --health-only
  ```

  Expected: all local scenarios pass; no real AI Work endpoint, public Core endpoint, or user Key is contacted.

- [ ] **Step 3: Inspect the release artifact and preserve the running public process**

  Verify the new executable hash, version metadata, database migration report, and health response. Do not stop or replace the current public process until the local release passes and a rollback copy is present under `D:\gpt`. Do not delete the existing release or data directory.

- [ ] **Step 4: Commit scripts and operator documentation**

  ```powershell
  git add scripts/test-starlink-public-deployment.mjs scripts/test-video-billing-state.mjs docs/core-foundation-operations.md docs/credit-aware-scheduler-operations.md
  git commit -m "test: add fail-closed video billing verification"
  ```

### Task 6: One controlled public diagnostic and final decision

**Files:**
- No product-code changes are allowed in this task unless the observed, task-bound receipt requires an explicit parser allowlist change; that change returns to Task 2 and is tested before redeployment.
- Evidence directory: `D:\gpt\aiwork-video-billing-acceptance`

**Interfaces:**
- Consumes: the locally verified release, public Core admin session, existing ordinary “周” Key used by the user, and one-time diagnostic claim.
- Produces: a redacted acceptance report with request ID, upstream task ID, status timestamps, receipt evidence classification, Core ledger result, and no plaintext secrets.

- [ ] **Step 1: Deploy the safety gate first and verify it publicly**

  Start the new release with the existing data/config paths, verify `/healthz`, verify ordinary text still passes its existing path, and verify both video entry points return `video_billing_paused` before any upstream call. Record only status codes and redacted IDs.

- [ ] **Step 2: Register exactly one diagnostic claim for the existing Key**

  Through the authenticated admin interface, bind the internal Key ID and exact request hash for the smallest non-reference-image video request. Do not create or rotate a Key. Confirm the control immediately returns to paused after the claim is consumed.

- [ ] **Step 3: Run exactly one real public request and poll it**

  Use the user-operated client with the existing “周” Key. Submit one 5-second, 720p, text-only Seedance task. Poll status through the public Core endpoint until terminal; do not retry, submit a second request, or use an aggregate balance delta as a charge. Capture the sanitized task response and the Core quota/ledger projection.

- [ ] **Step 4: Decide based on evidence**

  If the response contains a task-bound, explicit, repeatable whole-credit receipt below the hold, settle once and show the exact actual amount. If it contains a task-bound fractional receipt, store it unchanged as `reconcile_required` and proceed to Task 7 only after the receipt unit and a safe upper bound are independently verified. If no verifiable receipt exists, keep the task `reconcile_required`, keep the hold, leave ordinary video paused, and report that the upstream contract does not expose verifiable single-task credits. In every branch, do not change historical fixed-1 entries or silently refund/charge them.

- [ ] **Step 5: Write and protect the redacted report**

  Store the report under `D:\gpt\aiwork-video-billing-acceptance`, remove any secret-like values before reviewing it, and keep it out of Git. Remove only the temporary fake-test files created by this plan after verification; retain the acceptance MP4 and ledger evidence unless the user separately requests deletion.

### Task 7: Conditional fixed-point quota migration for verified fractional credits

Run this task only when Task 6 produced a repeatable task-bound receipt with an explicit upstream `credits` unit and a verified maximum request cost. If Task 6 produced no such receipt, leave this task unselected and keep ordinary video paused; the previous tasks still provide the safe failure behavior.

**Files:**
- Modify: `src-core/src/store.rs: schema migration and atomic unit-scale migration`
- Modify: `src-core/src/quota.rs: internal amount validation and balance calculations`
- Modify: `src-core/src/requests.rs: cost-policy amount conversion and settlement validation`
- Modify: `src-core/src/admin_summary.rs: display conversion for scaled amounts`
- Modify: `src-core/src/models.rs: expose scale/display metadata`
- Modify: `starlink-dimension-router/src/admin_routes.rs: whole-display-unit input/output conversion`
- Modify: `starlink-dimension-router/src/user_routes.rs: receipt-to-microcredit conversion`
- Modify: `starlink-dimension-router/static/index.html: display decimal credits without exposing internal scale`
- Test: `src-core/tests/credit_scale_migration.rs` (create)
- Test: `src-core/tests/key_quota_pool.rs`
- Test: `starlink-dimension-router/tests/video_billing.rs`

**Interfaces:**
- Consumes: stored `actual_credits` decimal strings from `UserVideoJob`, verified scale metadata, and existing quota/account ledger rows.
- Produces: `QUOTA_UNIT_SCALE = 1_000_000`, atomic `CoreStore::quota_unit_scale`, checked decimal-to-microcredit conversion, and exact fractional settlement with six decimal places.

- [ ] **Step 1: Write migration and conversion tests before changing the ledger**

  ```rust
  #[test]
  fn six_decimal_conversion_is_exact_and_over_precision_is_rejected() {
      assert_eq!(parse_microcredits("12.500000").unwrap(), 12_500_000);
      assert!(parse_microcredits("12.5000001").is_err());
      assert!(parse_microcredits("-1").is_err());
  }

  #[test]
  fn scale_migration_multiplies_all_quota_amounts_once_and_preserves_display_values() {
      let store = store_with_quota_rows(7, 3, 2);
      store.migrate_quota_to_microcredits().unwrap();
      assert_eq!(store.key_available("key-1", "credits").unwrap(), 7_000_000);
      assert_eq!(store.key_held("key-1", "credits").unwrap(), 3_000_000);
      assert_eq!(store.key_settled("key-1", "credits").unwrap(), 2_000_000);
      store.migrate_quota_to_microcredits().unwrap();
      assert_eq!(store.key_available("key-1", "credits").unwrap(), 7_000_000);
  }

  #[test]
  fn overflow_rolls_back_the_entire_scale_migration() {
      let store = store_with_max_i64_quota_row();
      assert!(store.migrate_quota_to_microcredits().is_err());
      assert_eq!(store.quota_unit_scale().unwrap(), 1);
  }
  ```

- [ ] **Step 2: Implement the atomic scale migration and checked conversion**

  Store the scale in `schema_meta` as `quota_unit_scale`, default `1`. In one `BEGIN IMMEDIATE` transaction, verify the receipt contract flag and maximum-cost policy, multiply `quota_ledger.amount`, `quota_ledger.delta`, `quota_reservations.amount`, `cost_policies.reserve_amount`, and `cost_policies.max_actual_amount` by `1_000_000` with checked arithmetic, then write the scale. Update all quota read/write paths to convert administrator-facing whole credits to internal microcredits and convert responses back to decimal strings. Do not scale `video_job` resource rows or upstream observations.

- [ ] **Step 3: Settle the stored fractional diagnostic task exactly once**

  After migration, run the existing idempotent settlement helper against the stored `actual_credits` string, convert it with `parse_microcredits`, verify it is within the migrated hold, commit it once, and mark the job `settled`. A second reconcile call must return the first final result without a new ledger row. The admin UI must show the exact decimal value and the internal scale only as metadata, never as a raw large integer.

- [ ] **Step 4: Run migration, quota, and router tests**

  ```powershell
  $env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
  cargo test -p aiwork-core --test credit_scale_migration --test key_quota_pool -- --nocapture
  cargo test -p starlink-dimension-router --test video_billing -- --nocapture
  ```

  Expected: PASS; old display balances are unchanged, fractional settlement is exact, rerunning migration is a no-op, overflow leaves the database untouched, and no other resource kind is scaled.

- [ ] **Step 5: Commit only after the migration report is reviewed**

  ```powershell
  git add src-core/src/store.rs src-core/src/schema.rs src-core/src/quota.rs src-core/src/requests.rs src-core/src/admin_summary.rs src-core/src/models.rs starlink-dimension-router/src/admin_routes.rs starlink-dimension-router/src/user_routes.rs starlink-dimension-router/static/index.html src-core/tests/credit_scale_migration.rs src-core/tests/key_quota_pool.rs starlink-dimension-router/tests/video_billing.rs
  git commit -m "feat: settle verified fractional video credits"
  ```

## Final verification command set

After all selected tasks are complete, run the project’s relevant Rust suites with the target directory on D:

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\traework-cargo-target'
cargo test -p aiwork-core --all-targets -- --nocapture
cargo test -p starlink-dimension-router --all-targets -- --nocapture
cargo test -p aiwork-assistant --all-targets -- --nocapture
```

Then run the no-paid-request local harness and the public `/healthz` check. Only after the final output shows the gate, settlement, migration, replay, and redaction tests passing may the result be described as fixed.

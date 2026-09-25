# Core Regular-Request Session Receipts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let ordinary Core-attributed text and video requests obtain a final receipt from one uniquely attributed TRAE usage session, without relaxing the pre-dispatch quote gate.

**Architecture:** AI Work already persists the authenticated Core `request_id → key_id` association and deterministic per-request upstream session. Replace the finalization path's one-shot-only lookup with a lookup of any authenticated Core request that has exactly one nonconflicting session. Keep the existing usage-history candidate checks, receipt idempotency, and Core-side quote requirement unchanged.

**Tech Stack:** Rust, Axum, SQLite, Cargo release tests.

**Spec:** `docs/superpowers/specs/2026-09-23-core-per-key-real-settlement-and-key-management-design.md`, sections 2, 4, 5, 9 and 10. This plan implements only the remaining AI Work receipt promotion portion; it does not invent a quote source or enable normal paid dispatch.

## Global Constraints

- A final receipt needs one immutable Core request ID, its stored Key ID, one nonconflicting upstream account/session pair, and one matching usage-history row in `credits` with at most six decimals.
- Missing, shared, conflicting, or multiple sessions remain `unknown`; no balance delta, token count, duration, or fixed amount becomes a receipt.
- A final receipt is immutable and idempotent; conflicting amounts or task references must not become a second charge.
- Tests and caches stay under `D:\gpt`; do not read/write production databases in tests. Keep the public paid-request quote gate closed.

## Review Focus

- Ordinary Core request with one unique session and matching usage row returns its actual decimal credits: Task 1 and Task 2 tests.
- Two Keys or requests sharing one upstream session cannot each claim it: Task 1 test.
- A request with two upstream attempts stays unknown even if one usage row exists: Task 1 and Task 2 tests.
- Absent request association or a mismatched Key/account/session cannot promote usage: Task 1 test.
- Repeated finalization yields the same receipt and a changed task reference or amount causes conflict: Task 1 and Task 2 tests.

---

### Task 1: Trust One Unique Authenticated Core Session

**Files:**
- Modify: `src-tauri/src/api_server/bridge_billing.rs`
- Test: `src-tauri/src/api_server/bridge_billing.rs` (`mod tests`)

**Interfaces:**
- Consumes: `BridgeBillingStore::record_core_request`, `record_core_session_attempt`, `BillingReceipt`, `CreditAmount`.
- Produces: `core_session_for_request(request_id) -> Result<CoreBillingSessionLookup, String>` and `record_core_session_receipt(receipt, account_ref, session_id, core_key_id) -> Result<PersistReceiptResult, String>`; the finalizer passes whether this is chat or video through the receipt's `task_ref`.

- [ ] **Step 1: Write failing tests.** In the existing `mod tests`, persist an ordinary Core request, then its unique session. Call the existing one-shot receipt method first so red proves the authorization restriction, and assert the desired replacement method's result after changing the test to its target interface:

```rust
store.record_core_request("req-regular", "key_b").unwrap();
store.record_core_session_attempt("req-regular", "uid-b", "session-b").unwrap();
let receipt = BillingReceipt {
    request_id: "req-regular".into(),
    status: BillingReceiptStatus::Final,
    actual_credits: Some(CreditAmount::parse("3.250000", "credits").unwrap()),
    unit: Some("credits".into()),
    source_ref: Some("trae-usage-session:session-b".into()),
    task_ref: Some("video-task-b".into()),
    observed_at_ms: 1_790_000_000_000,
};
assert_eq!(store.record_core_session_receipt(&receipt, "uid-b", "session-b", "key_b").unwrap(), PersistReceiptResult::Created);
```

Add separately named tests proving that a missing request, wrong Key, wrong account/session, session reused by two requests, a second attempt, and a changed final receipt stay rejected or conflicted. A chat receipt with `task_ref: None` and `0.050400` credits must also finalize.
- [ ] **Step 2: Verify red.** Run `cargo test --release --locked --manifest-path src-tauri/Cargo.toml api_server::bridge_billing::tests -- --test-threads=2` with `CARGO_HOME=D:\gpt\aiwork-cargo-home`, `CARGO_TARGET_DIR=D:\gpt\aiwork-production-target`, `TEMP=TMP=D:\gpt\aiwork-npm-cache`, and `AIWORK_ASSET_DIR` removed only from this test process. Expected: the new ordinary-request test fails before the generic method exists.
- [ ] **Step 3: Minimal implementation.** The lookup must read `bridge_core_requests` for the immutable Key/conflict bit and `bridge_core_upstream_sessions` for the unique account/session, returning `Missing` or `Ambiguous` otherwise. `record_core_session_receipt` must compare all four IDs, then choose the video or chat trust validation from `receipt.task_ref`; it must not promote arbitrary HTTP input. Keep the one-shot authorization table for admission and diagnostics only.
- [ ] **Step 4: Verify green.** Rerun the exact Task 1 test command; expected: all `bridge_billing::tests` pass, including the original one-shot path.
- [ ] **Step 5: Commit.** Run `git add src-tauri/src/api_server/bridge_billing.rs` then `git commit -m "fix: finalize uniquely attributed Core usage sessions"`.

### Task 2: Finalize Ordinary Chat and Video Through the Same Evidence Path

**Files:**
- Modify: `src-tauri/src/api_server/bridge_api.rs`
- Test: `src-tauri/src/api_server/server.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 1's `core_session_for_request` and `record_core_session_receipt`; existing `core_usage_receipt_candidate` checks the cached usage row against account, session, request and Key.
- Produces: existing `POST /internal/bridge/requests/{request_id}/billing/finalize-chat` and `/billing/finalize` responses, now also final for ordinary Core requests with exact evidence.

- [ ] **Step 1: Write failing route tests.** Reuse `BridgeFixture`; call `persist_core_request_attribution(&fixture.dir, "request-chat-regular", "key_chat")`, persist `("request-chat-regular", "uid-chat", "session-chat")`, write the same fully attributed usage-cache shape as the adjacent one-shot chat test, and POST `/internal/bridge/requests/request-chat-regular/billing/finalize-chat`. Assert `status=final`, `actual_credits="0.050400"`, and a repeated POST returns the identical receipt. Do the equivalent for video with `task_ref="video-task-a"` and `3.250000` credits. Add unknown assertions for absent or ambiguous session. Existing one-shot tests must remain green.
- [ ] **Step 2: Verify red.** Run `cargo test --release --locked --manifest-path src-tauri/Cargo.toml api_server::server::tests -- --test-threads=2` with the same D-only environment. Expected: ordinary request finalization returns `unknown`, not the test's `final`.
- [ ] **Step 3: Minimal implementation.** Switch both finalizers to Task 1's `core_session_for_request` and `record_core_session_receipt`; preserve `unknown` for missing/ambiguous evidence and `billing_store_unavailable` for store failure. Do not change quote, normal request dispatch, or Core balance code.
- [ ] **Step 4: Verify green.** Rerun Task 2 tests, then `cargo test --release --locked --manifest-path src-tauri/Cargo.toml -- --test-threads=2`; expected: zero failures. Check `git diff --check` and repository status.
- [ ] **Step 5: Commit and release.** Commit the two route/test files with `git commit -m "fix: finalize regular Core chat and video receipts"`. Build with `cargo build --release --locked --manifest-path src-tauri/Cargo.toml` to the D target. Deploy as a new versioned local AI Work release only after tests pass, confirm `http://127.0.0.1:7864/health` and Core's refreshed upstream snapshot, then push the AI Work branch. Keep the old executable for rollback and clean only verified generated D temporary directories.

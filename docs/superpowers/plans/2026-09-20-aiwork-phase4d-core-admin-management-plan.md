# Phase 4D Core Admin Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 把现有 Core 管理能力接成安全、可验证的 Tauri Core 管理界面，支持用户、Key、永久额度和调度状态管理。

**Architecture:** Core 负责管理员 Principal 授权、只读投影和事务性写入；Tauri commands 只做输入/错误适配；React 管理 Tab 只保存当前会话的 admin Key，不持久化凭据。legacy `api_keys.json` 管理保持原样，两套身份与额度账本不合并。

**Tech Stack:** Rust 2024、rusqlite、Tauri 2 commands、React 18、TypeScript、Vitest、现有 `Modal`/`Badge` UI primitives。

**Spec:** `docs/superpowers/specs/2026-09-20-aiwork-phase4d-core-admin-management-design.md`

## Global Constraints

- Core admin read/write operations must authorize the authenticated `Principal` inside the transaction.
- Never return or persist API key plaintext after issuance; never expose key digest, credentials, upstream account identity, or another user’s task/asset data.
- Disabling a user must not delete quota ledger, reservations, jobs, attempts, assets, or audit events.
- Tests and Cargo targets/logs/fixtures use `D:\gpt`; Cargo uses `--offline --locked`; no real network or upstream accounts.
- Preserve pre-existing dirty files and do not stage `src-core/Cargo.lock`, `src-tauri/target-fix/`, `data/`, `credentials/`, or unrelated legacy UI changes.

## Review Focus

- A normal user Key or a string such as `admin` must never authorize an admin command; test the real Core authentication path.
- The last active admin and the current admin cannot be disabled; test both races through the same immediate transaction.
- API key list responses must contain only prefix/metadata, never plaintext or digest; test serialized output.
- Repeated revoke and repeated quota reads must be idempotent and must not add ledger entries or mutate state.
- Closing/reopening the React modal must clear the admin Key and must not write it to the persistent app store.

---

### Task 1: Core admin projections and guarded mutations

**Files:**
- Modify: `src-core/src/models.rs`
- Modify: `src-core/src/store.rs`
- Modify: `src-core/src/error.rs` only if a precise non-sensitive error variant is required
- Test: `src-core/tests/identity.rs`, `src-core/tests/quota.rs`

**Interfaces:**
- Consumes: existing `Principal`, `users`, `api_keys`, `quota_ledger`, `quota_reservations`, and `authorize_admin_principal_in_transaction`.
- Produces: `CoreUserAdminView`, `CoreApiKeyAdminView`, `list_users_as_admin`, `list_api_keys_as_admin`, `quota_balance_as_admin`, `set_user_status_as_admin`, and principal-based `revoke_api_key_as_admin`.

- [ ] **Step 1: Write failing tests** for admin-only projections, redacted key fields, self/last-admin disable rejection, unknown target rejection, repeat revoke, and quota balance authorization.
- [ ] **Step 2: Run the focused Core tests** with `TEMP/TMP=D:\gpt`, `--offline --locked`; record the expected compile/missing-method failures.
- [ ] **Step 3: Add the models and implement each query/write in an immediate transaction**, reusing existing authorization and audit helpers. The user status update must lock the transaction, reject self/last-active-admin disable, update timestamps, and audit the action.
- [ ] **Step 4: Run the focused tests** and verify all red tests become green without returning key digest/plaintext.
- [ ] **Step 5: Commit** with `feat: expose guarded core admin projections`.

### Task 2: Tauri Core admin command surface

**Files:**
- Modify: `src-tauri/src/commands/core.rs`
- Modify: `src-tauri/src/main.rs`
- Test: `src-tauri/src/commands/core.rs` module tests

**Interfaces:**
- Consumes: Task 1 Core methods and existing `authenticate_admin`/`core_store_for_admin`.
- Produces: `core_users_list`, `core_api_keys_list`, `core_quota_balance`, `core_user_set_status`, and `core_api_key_revoke` Tauri commands plus serializable response types.

- [ ] **Step 1: Write failing command tests** for empty/invalid/non-admin keys, successful list/read/write paths, command registration, and serialized redaction.
- [ ] **Step 2: Run the focused Tauri test filter** on `D:\gpt\aiwork-phase4d-tauri` with `--offline --locked` and observe failure.
- [ ] **Step 3: Implement thin command adapters** that authenticate the admin Key once, validate non-empty ids/resource kinds/reasons, map Core models to serializable responses, and never log or persist the supplied admin Key.
- [ ] **Step 4: Register all commands in `main.rs`** and rerun focused tests.
- [ ] **Step 5: Commit** with `feat: add tauri core admin commands`.

### Task 3: Desktop Core 管理 Tab

**Files:**
- Modify: `src/types.ts`
- Modify: `src/lib/tauri.ts`
- Create: `src/components/api/CoreAdminPanel.tsx`
- Modify: `src/components/api/ApiManagerModal.tsx`
- Test: `src/components/api/CoreAdminPanel.test.tsx`

**Interfaces:**
- Consumes: Task 2 command names and response shapes; existing `Modal`, `Badge`, `useAppStore`, and `api` wrapper.
- Produces: a `Core 管理` tab with status, users, keys, quota grant, revoke and status controls.

- [ ] **Step 1: Write failing component tests** with mocked `invoke`: admin Key is sent only to commands, close/reopen clears it, lists render redacted fields, and rejection/error states remain safe.
- [ ] **Step 2: Run the focused Vitest test** and observe the missing component/API wrapper failure.
- [ ] **Step 3: Add response types and `api.core` wrappers**, then implement the panel with explicit loading/error states, controlled admin Key state, one-time issued-key display/copy, and existing modal confirmation patterns.
- [ ] **Step 4: Add the tab and run focused Vitest plus `npm run build`** with `TEMP/TMP=D:\gpt`.
- [ ] **Step 5: Commit** with `feat: add core admin management tab`.

### Task 4: Documentation and full verification

**Files:**
- Modify: `AGENT.md`
- Modify: `docs/credit-aware-scheduler-operations.md`
- Modify: `docs/user-manual.md`

**Interfaces:**
- Consumes: Tasks 1–3 command/UI behavior and verification output.
- Produces: operator instructions that distinguish Core permanent quota from legacy daily limits and explicitly document admin-key handling.

- [ ] **Step 1: Update docs** with exact command/UI workflow, redaction rules, rollback and current limitation that no公网 admin API or online payment exists.
- [ ] **Step 2: Run Core focused/full, Tauri focused/full, Vitest, `npm run build`, `git diff --check`, and inspect staged file list**; keep every target/log on `D:\gpt`.
- [ ] **Step 3: Commit** with `docs: document core admin management`.
- [ ] **Step 4: Re-read the spec and plan, verify every review-focus item from current output, and report Phase 4D evidence plus remaining Phase 4/5 gaps. Do not mark the overall goal complete.**

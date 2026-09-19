# Core User Usage Query Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在已批准的方案 B Core 鉴权边界内提供 `usage:read` 用户自助额度查询，不泄露其他用户、凭据或请求正文。

**Architecture:** CoreStore 在同一只读事务中验证当前 Principal、汇总该用户的所有资源额度和有限流水；Tauri/Axum 网关只把已安装的 Core Principal 映射为 `/v1/usage` JSON 响应。非 Core enforce、缺 scope、越界 limit 和存储错误均采用稳定错误，不回退 legacy 账本。

**Tech Stack:** Rust 2021, `aiwork-core`, SQLite/rusqlite, Axum 0.7, serde/serde_json, existing bearer-auth middleware.

**Spec:** `docs/superpowers/specs/2026-09-19-aiwork-unified-gateway-design.md`（`usage:read` scope、用户级 quota/ledger 隔离和不暴露敏感字段）。

## Global Constraints

- 只使用认证 middleware 注入的 `Principal`，忽略请求体、查询参数和自定义 header 中的用户身份。
- 只读查询必须限定 `principal.user_id`；不得返回 `actor_user_id`、prompt、输入摘要、凭据、上游账号或完整请求内容。
- `available`、`held`、`settled` 使用 Core 整数逻辑单位；不把上游余额或积分换算成用户额度。
- Core `off`/`shadow` 不写第二本账；`/v1/usage` 在非 enforce 模式明确返回 501。
- limit 默认 100，允许范围 1..=100；所有测试、日志和临时目录继续使用 `D:\gpt`，不在 C 盘写测试产物。
- 保留当前用户未提交改动；只提交本计划新增/明确修改的文件，不提交 `src-core/Cargo.lock`、`src-tauri/target-fix/` 或数据/凭据。

## Review Focus

- 恶意 Principal 或 Key 只能读自己的余额和流水：由 Task 1 的真实 Core 主体校验与 Task 2 的 route scope 测试覆盖。
- `unknown`/`held` reservation 必须仍计入 `held`，不得被查询逻辑当作已消费或自动退款：由 Task 1 的结算夹具覆盖。
- 没有 Core、Core 为 shadow/off、缺 scope 或 limit 越界时不能进入 legacy 路径：由 Task 2 的响应映射和路由注册测试覆盖。
- 流水中不能出现 actor、reason、prompt、digest、凭据或上游字段：由 Task 1 的序列化断言覆盖。
- 同一时间戳的流水仍需稳定排序且响应有硬上限：由 Task 1 的 limit/order 断言覆盖。

### Task 1: Core 用户额度与脱敏流水投影

**Files:**
- Modify: `src-core/src/models.rs`（新增可序列化 `CoreQuotaUsageView`、`CoreQuotaBalanceView`、`CoreQuotaLedgerView`）
- Modify: `src-core/src/lib.rs`（导出三个 view 类型）
- Modify: `src-core/src/quota.rs`（新增 `CoreStore::quota_usage_for_principal`）
- Test: `src-core/tests/quota.rs`

**Interfaces:**
- Consumes: 已有 `Principal`、`ensure_principal_in_transaction`、`quota_ledger`、`quota_reservations` 和 `balance_in_transaction`。
- Produces: `pub fn quota_usage_for_principal(&self, principal: &Principal, limit: usize) -> Result<CoreQuotaUsageView, CoreError>`；view 只包含 `balances` 和 `ledger`。

- [ ] **Step 1: Write the failing test**

在 `src-core/tests/quota.rs` 增加一个真实 SQLite 夹具：为用户授予额度，创建一笔 held、unknown 和 committed reservation，再用另一个用户写入相似 resource；断言当前 Principal 只能看到自己的 resource、`held` 包含 held/unknown、`settled` 只累计 commit amount，流水 JSON 不包含 `actor_user_id`、`reason`、其他用户 id 或 request body；同时断言 limit=0 和 101 返回 `CoreError::Validation`。

- [ ] **Step 2: Run the focused test to verify red**

Run:

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-user-usage-task1-red --offline --locked --test quota user_quota_usage_projection -- --nocapture
```

Expected: FAIL because `quota_usage_for_principal` and the three projection types do not exist.

- [ ] **Step 3: Implement the minimal Core projection**

Add the following behavior in `src-core/src/quota.rs`: validate `1..=100`, open a deferred read transaction, call `ensure_principal_in_transaction`, select distinct resource kinds from the caller's ledger/reservations, compute `available`, `held` (states `held` and `unknown`) and `settled` (`event_kind='commit'` amount sum), then select at most `limit` newest ledger rows ordered by `created_at_ms DESC, entry_id DESC`. Map only `resource_kind`, `event_kind`, `amount`, `delta`, `request_id`, and `created_at_ms`; never serialize actor/reason/entry ids.

- [ ] **Step 4: Run the focused and full Core tests**

Run the focused test again, then:

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-user-usage-task1-green --offline --locked --tests
```

Expected: the new projection test and every existing Core target pass.

- [ ] **Step 5: Commit Task 1**

```powershell
git add src-core/src/models.rs src-core/src/lib.rs src-core/src/quota.rs src-core/tests/quota.rs
git commit -m "feat: expose user quota usage projection"
```

### Task 2: Expose the authenticated `/v1/usage` route

**Files:**
- Create: `src-tauri/src/api_server/core_account.rs`
- Modify: `src-tauri/src/api_server/mod.rs`（注册模块）
- Modify: `src-tauri/src/api_server/server.rs`（注册 `GET /v1/usage`）
- Test: `src-tauri/src/api_server/core_account.rs` and `server.rs` route-registration tests

**Interfaces:**
- Consumes: Task 1 `CoreStore::quota_usage_for_principal`, existing `auth::bearer_auth` Principal extension and `usage:read` scope.
- Produces: `GET /v1/usage?limit=<1..100>` with response `{object:"user_usage", balances:[...], ledger:[...], limit:<n>}`.

- [ ] **Step 1: Write the failing route contract tests**

Add pure response tests for missing scope, non-enforce Core, invalid limit and successful projection mapping; add a `server.rs` source/Router test asserting the exact `/v1/usage` route is registered before middleware. The success fixture must use a Principal and Core view from Task 1 and assert no user/actor/prompt/credential fields are emitted.

- [ ] **Step 2: Run the focused Tauri tests to verify red**

Run:

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-user-usage-task2-red --offline --locked usage_ -- --nocapture
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-user-usage-task2-red --offline --locked router_registers_the_authenticated_user_usage_route -- --nocapture
```

Expected: FAIL because the module, handler, and route are not registered.

- [ ] **Step 3: Implement the minimal handler and route**

Create `core_account::usage` with `State<Arc<ApiSharedState>>`, `Extension<Principal>` and `Query<UsageQuery>`. Require `usage:read`; require `CoreMode::Enforce`; call the Core projection with default 100. Map errors to `401 unauthorized`, `403 insufficient_scope`, `400 invalid_usage_limit`, `501 core_usage_not_enabled`, or generic `500 core_error`; never include the underlying storage error in the public body. Add `pub mod core_account` and `.route("/v1/usage", get(core_account::usage))` to the existing router without bypassing auth/CORS layers.

- [ ] **Step 4: Run focused tests and the full Tauri suite**

Run both focused commands above again, then:

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'; & 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --target-dir D:\gpt\aiwork-user-usage-tauri-green --offline --locked -- --nocapture
```

Expected: focused route tests pass and the full Tauri suite remains green with no real network/upstream call.

- [ ] **Step 5: Commit Task 2**

```powershell
git add src-tauri/src/api_server/core_account.rs src-tauri/src/api_server/mod.rs src-tauri/src/api_server/server.rs
git commit -m "feat: add authenticated user usage endpoint"
```

### Task 3: Contract documentation and delivery verification

**Files:**
- Modify: `docs/superpowers/specs/2026-09-19-aiwork-unified-gateway-design.md`（补充 `/v1/usage` 兼容矩阵和脱敏响应契约）
- Modify: `docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-operations.md`（补充用户查询边界）
- Test: static contract/diff checks and frontend `npm test` regression

**Interfaces:**
- Consumes: Task 2 route contract.
- Produces: 可审查的用户额度查询说明，明确 Core-only、scope、limit、unknown hold 和不支持上游余额换算。

- [ ] **Step 1: Write the documentation assertions first**

Use this exact read-only assertion command as the documentation test; it requires every contract marker to occur in the two target documents and exits non-zero while the contract is absent:

```powershell
$docs = @('docs/superpowers/specs/2026-09-19-aiwork-unified-gateway-design.md','docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-operations.md')
$need = @('GET /v1/usage','usage:read','held','settled','unknown','上游余额不转换为用户额度')
$text = (Get-Content -LiteralPath $docs[0] -Raw) + (Get-Content -LiteralPath $docs[1] -Raw)
foreach ($marker in $need) { if ($text -notlike "*$marker*") { throw "missing documentation marker: $marker" } }
```

- [ ] **Step 2: Run the check to verify red**

Run the exact PowerShell assertion above with output captured at `D:\gpt\aiwork-user-usage-docs-red.log`. Expected: FAIL because the two documents do not yet describe `/v1/usage` and its `usage:read` contract.

- [ ] **Step 3: Update the two documents with the exact route contract**

Document authentication, response fields, bounded limit, owner isolation, unknown/held semantics, Core mode behavior and the fact that upstream balances are not converted to user quota. Do not claim real upstream billing or public deployment verification.

- [ ] **Step 4: Run the complete delivery checks**

Run the documentation check again, `npm test`, `git diff --check`, and inspect `git status` to confirm only this plan's paths are staged. All Cargo logs/targets remain under `D:\gpt`; do not run broad tests with inherited `AIWORK_*` or deployment overrides.

- [ ] **Step 5: Commit Task 3**

```powershell
git add docs/superpowers/specs/2026-09-19-aiwork-unified-gateway-design.md docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-operations.md
git commit -m "docs: specify user usage query contract"
```

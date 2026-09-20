# 星链维度分流系统实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 CORE 拆分为独立的“星链维度分流系统”程序和 API，使 AI Work 仅承担上游执行与管理员中转 Key，普通用户 Key、额度和任务归属全部由 CORE 管理。

**Architecture:** 新增不依赖 Tauri 的 Rust `starlink-dimension-router` 服务，复用 `src-core` 的 SQLite 账本和身份模型；服务同时提供用户兼容 API、管理员 API 和独立管理界面。AI Work 增加桥接专用模式和脱敏桥接接口，所有 CORE 转发都使用唯一管理员中转 Key，普通用户 Key 不进入 AI Work。

**Tech Stack:** Rust 2021、Axum 0.7、Tokio、SQLite/rusqlite、`aiwork-core`、现有 React/Vite/Tauri 管理界面组件、PowerShell 构建脚本。

**Spec:** `docs/superpowers/specs/2026-09-21-starlink-dimension-router-design.md`

## Global Constraints

- AI Work 默认执行端端口保持 7864；星链维度分流系统使用可配置默认端口 7865。
- AI Work 在桥接专用模式只接受当前有效管理员中转 Key；普通用户 Key 不能绕过 CORE 调用执行入口。
- 普通用户 Key 只能由星链维度分流系统创建、撤销和查询；AI Work 只允许创建或轮换桥接管理员 Key。
- CORE 用户额度与 AI Work 上游真实积分分别记账，不按 token 推算积分，不自动按日恢复永久额度。
- 管理响应不得包含账号池、底层 UID、Cookie、JWT、单账号积分、内部文件路径或上游授权 URL。
- bridge test 失败不得保存未验证配置；bridge sync 失败保留上一次可信快照并标记过期。
- 未知上游提交结果进入待对账，禁止自动换号、重放或未经授权退款。
- 测试使用 Mock/Fake 执行端，不发送真实上游文字或视频请求。
- Rust 构建、Cargo 临时目录、测试文件和打包输出优先使用 `D:\gpt`；不在 C 盘进行持续读写测试。
- 每个任务先写一个可失败的测试，再写最小实现；每个任务通过验证后单独提交。
- 不重置、清理或覆盖当前工作树中与本计划无关的用户修改。

## Review Focus

1. 普通用户 Key 被直接发往 AI Work 或伪造 `X-Core-Request-Id` 时，AI Work 必须拒绝或忽略伪造身份；测试归 Task 2/4。
2. bridge URL、Key、模型同步失败时，旧快照和未验证密钥必须保留/不落盘；测试归 Task 3/5。
3. 管理汇总必须只显示聚合指标，不能通过错误字段或调试响应泄露账号池；测试归 Task 3/6。
4. 同一用户、同一幂等键重复转发时不得重复预占、重复执行或重复结算；测试归 Task 4/5。
5. 迁移中途失败、AI Work 离线或费用未知时，数据必须可恢复，不能删除旧数据或自动重放；测试归 Task 7/8。

---

### Task 1: 建立独立服务骨架与共享契约

**Files:**
- Create: `starlink-dimension-router/Cargo.toml`
- Create: `starlink-dimension-router/src/lib.rs`
- Create: `starlink-dimension-router/src/main.rs`
- Create: `starlink-dimension-router/src/config.rs`
- Create: `starlink-dimension-router/src/dto.rs`
- Test: `starlink-dimension-router/src/config.rs`
- Test: `starlink-dimension-router/src/dto.rs`

**Interfaces:**
- Produces `RouterConfig::load(data_dir)`, `RouterConfig::default_port()`, `BridgeConfig`, `CoreAdminSummary`, `BridgeStatusSnapshot`。
- `StarlinkRouterState` 后续任务使用 `Arc<CoreStore>`、`BridgeClient` 和配置快照，不依赖 `src-tauri` crate。

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn router_defaults_to_port_7865_and_separate_data_directory() {
    let config = RouterConfig::defaults(PathBuf::from(r"D:\gpt\starlink-dimension-router-data"));
    assert_eq!(config.port, 7865);
    assert_eq!(config.display_name, "星链维度分流系统");
}

#[test]
fn bridge_secret_is_never_serialized_in_public_status() {
    let status = BridgeStatusSnapshot::connected("https://127.0.0.1:7864", "seedance", 3);
    let json = serde_json::to_string(&status).unwrap();
    assert!(!json.contains("Authorization"));
    assert!(!json.contains("api_key"));
}
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\starlink-router-cargo-target'
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
```

Expected: FAIL because the new crate, configuration and DTOs do not exist。

- [ ] **Step 3: Implement the minimal service types**

Add a standalone package depending on `aiwork-core = { path = "../src-core" }`, `axum`, `tokio`, `serde`, `serde_json`, `chrono`, `sha2`, `base64`, and `ureq`. `RouterConfig` must read `STARLINK_ROUTER_DATA_DIR`, `STARLINK_ROUTER_HOST`, `STARLINK_ROUTER_PORT`, and an optional persisted JSON config, with explicit validation for host, port, and absolute data directory. Store only a protected bridge-key reference in config DTOs; never include the plaintext key in `Serialize` implementations.

- [ ] **Step 4: Run tests and verify GREEN**

Run the same targeted command. Expected: configuration and serialization tests pass。

- [ ] **Step 5: Commit**

```powershell
git add starlink-dimension-router/Cargo.toml starlink-dimension-router/src
git commit -m "feat: scaffold standalone starlink router"
```

### Task 2: Restrict AI Work to bridge execution mode

**Files:**
- Modify: `src-tauri/src/api_server/api_keys.rs`
- Modify: `src-tauri/src/api_server/auth.rs`
- Modify: `src-tauri/src/api_server/gateway_settings.rs`
- Modify: `src-tauri/src/api_server/server.rs`
- Modify: `src-tauri/src/commands/api_server.rs`
- Modify: `src-tauri/src/main.rs`
- Modify: `src/components/api/ApiKeysManager.tsx`
- Modify: `src/components/api/ApiKeysManager.test.tsx`
- Test: `src-tauri/src/api_server/api_keys.rs`, `src-tauri/src/api_server/auth.rs`

**Interfaces:**
- Produces `BridgeKeyKind`, `issue_bridge_key`, `revoke_bridge_key`, `active_bridge_key`, and `bridge_only` gateway setting。
- `bearer_auth` receives the bridge-only decision before the legacy capability check and rejects non-bridge keys for business routes。
- Tauri commands expose bridge-key status/issue/revoke only; ordinary legacy-key creation is hidden or presented as migration-only read-only information。

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn bridge_key_rotation_leaves_only_one_active_bridge_key() {
    let first = file.issue_bridge_key(&data_dir, "bootstrap").unwrap();
    let second = file.issue_bridge_key(&data_dir, "rotate").unwrap();
    assert!(file.find_active_bridge_key(&first.id).unwrap().revoked);
    assert!(!file.find_active_bridge_key(&second.id).unwrap().revoked);
}

#[test]
fn bridge_only_rejects_a_legacy_user_key_before_dispatch() {
    let response = authenticate_bridge_only(&legacy_key, false);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
```

- [ ] **Step 2: Run the focused Rust tests and verify RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\starlink-router-cargo-target'
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --offline --locked api_server::api_keys api_server::auth
```

Expected: FAIL because the bridge key kind and bridge-only authorization do not exist。

- [ ] **Step 3: Implement bridge-only authorization and UI restriction**

Extend the existing key JSON schema with a backward-compatible key kind (`legacy` or `bridge`) and a bridge-only gateway flag. Issuing a bridge key revokes the prior active bridge key, returns plaintext once, and stores only its existing secure representation. Make `bearer_auth` require the bridge kind for `/internal/bridge/*` and, when bridge-only is enabled, for forwarded `/v1/*` traffic. Preserve the current health endpoint behavior and do not apply bridge-only rules to local migration commands. Replace ordinary API-key creation controls in `ApiKeysManager` with a bridge-key panel showing status, last rotation, scope summary, one-time plaintext result, and a migration notice.

- [ ] **Step 4: Run focused and existing API-key/UI tests**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --offline --locked api_server::api_keys api_server::auth
npm.cmd test -- --run src/components/api/ApiKeysManager.test.tsx
```

Expected: new bridge-only tests and existing key compatibility tests pass。

- [ ] **Step 5: Commit**

```powershell
git add src-tauri/src/api_server/api_keys.rs src-tauri/src/api_server/auth.rs src-tauri/src/api_server/gateway_settings.rs src-tauri/src/api_server/server.rs src-tauri/src/commands/api_server.rs src-tauri/src/main.rs src/components/api/ApiKeysManager.tsx src/components/api/ApiKeysManager.test.tsx
git commit -m "feat: add ai work bridge-only mode"
```

### Task 3: Add sanitized AI Work bridge endpoints

**Files:**
- Create: `src-tauri/src/api_server/bridge_api.rs`
- Modify: `src-tauri/src/api_server/server.rs`
- Modify: `src-tauri/src/api_server/routes.rs`
- Modify: `src-tauri/src/api_server/mod.rs`
- Modify: `src-tauri/src/commands/api_server.rs`
- Test: `src-tauri/src/api_server/bridge_api.rs`

**Interfaces:**
- `GET /internal/bridge/status` returns `BridgeStatusResponse`。
- `GET /internal/bridge/models` returns the public model/capability catalog。
- `GET /internal/bridge/summary` returns `BridgeSummaryResponse` containing only aggregate execution and upstream-credit fields。
- `BridgeSummaryResponse` must not contain account IDs, UID, account names, cookies, JWTs, file paths, or per-account balances。
- Test helpers created in Step 1: `fake_state() -> Arc<ApiSharedState>` uses only deterministic in-memory/mock values; `bridge_summary_for_test(state) -> Response` calls the new summary handler; `read_json(response) -> serde_json::Value` consumes the body with `axum::body::to_bytes` and parses JSON。

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn bridge_summary_contains_aggregates_but_no_pool_rows() {
    let response = bridge_summary_for_test(fake_state()).await;
    let body = read_json(response).await;
    assert_eq!(body["active_accounts"], serde_json::Value::Null);
    assert_eq!(body["active_models"], 2);
    assert!(body.get("accounts").is_none());
    assert!(body.get("uids").is_none());
    assert!(body.get("credits_by_account").is_none());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run the bridge API test with the D-drive Cargo target. Expected: FAIL because the route and sanitized DTO do not exist。

- [ ] **Step 3: Implement the bridge routes**

Add a separate bridge authentication layer that accepts only the active bridge key. Derive model data from the existing unified catalog. Derive upstream points as one aggregate value with source and `updated_at`; if the source is unavailable, return `value: null`, `fresh: false`, and a stable error code rather than zero. Return active CORE-facing capabilities, current execution availability, and aggregate task counts only. Keep the bridge route on the existing AI Work listener so the CORE can use either LAN or HTTPS public Base URL.

- [ ] **Step 4: Verify routes and redaction**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --offline --locked api_server::bridge_api api_server::routes
```

Expected: bridge status/model/summary tests pass and redaction assertions contain no pool fields。

- [ ] **Step 5: Commit**

```powershell
git add src-tauri/src/api_server/bridge_api.rs src-tauri/src/api_server/server.rs src-tauri/src/api_server/routes.rs src-tauri/src/api_server/mod.rs src-tauri/src/commands/api_server.rs
git commit -m "feat: expose sanitized ai work bridge API"
```

### Task 4: Implement the standalone CORE user API and bridge forwarding

**Files:**
- Modify: `starlink-dimension-router/src/lib.rs`
- Create: `starlink-dimension-router/src/state.rs`
- Create: `starlink-dimension-router/src/auth.rs`
- Create: `starlink-dimension-router/src/bridge_client.rs`
- Create: `starlink-dimension-router/src/server.rs`
- Create: `starlink-dimension-router/src/user_routes.rs`
- Create: `starlink-dimension-router/src/admin_auth.rs`
- Test: `starlink-dimension-router/src/auth.rs`, `starlink-dimension-router/src/server.rs`, `starlink-dimension-router/src/user_routes.rs`

**Interfaces:**
- `StarlinkRouterState { store: Arc<CoreStore>, bridge: BridgeClient, config: RouterConfig }`。
- `BridgeClient::test()`, `BridgeClient::models()`, `BridgeClient::summary()`, and `BridgeClient::forward()` always overwrite the Authorization header with the bridge secret。
- `require_core_principal` authenticates only the CORE user Key and returns a `Principal`。
- User routes preserve the existing `/v1/*` response shape while recording CORE request/task IDs。
- Test helpers created in Step 1: `FakeBridge` implements the local bridge trait and records the final Authorization/request ID; `funded_user(key) -> TestPrincipal` creates one Core user/key with a deterministic balance; `test_router(bridge, principal) -> Router`; `request(router, method, path, key, body) -> Response`; `chat_body() -> Bytes`; `submit_video(router) -> Response`; `latest_job_state() -> &'static str` reads the test store after the request。

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn user_key_is_replaced_by_bridge_key_when_forwarding() {
    let fake = FakeBridge::recording();
    let app = test_router(fake.clone(), funded_user("user-key"));
    let response = request(app, "POST", "/v1/chat/completions", "user-key", chat_body()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.last_authorization(), Some("Bearer bridge-secret".into()));
    assert_ne!(fake.last_request_id(), Some("client-forged".into()));
}

#[tokio::test]
async fn unknown_bridge_result_enters_reconcile_state_without_retry() {
    let fake = FakeBridge::transport_unknown();
    let response = submit_video(test_router(fake, funded_user("user-key"))).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(latest_job_state(), "reconcile_required");
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\starlink-router-cargo-target'
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
```

Expected: FAIL because the standalone router, bridge client and user routes do not exist。

- [ ] **Step 3: Implement authentication, forwarding and reservation flow**

Open the existing `CoreStore` database in the configured standalone data directory and authenticate the user Key against it. For each request, derive capability from path/model, call the store’s authoritative quota/lease methods, persist the request before forwarding, and use a user-scoped idempotency key. The bridge client uses the saved AI Work URL and bridge secret, copies only safe headers, overwrites `Authorization`, `X-Core-Request-Id`, and internal user headers, and uses bounded request/response sizes and timeouts. `seedance` Chat requests use the same projection and asynchronous response contract already verified in AI Work. User-facing video status and content routes enforce task ownership in CORE before calling the bridge.

- [ ] **Step 4: Run focused tests and compatibility checks**

Run the new standalone tests plus the existing Core flow tests. Expected: ordinary text and Seedance video paths, ownership checks, scope failures, quota failures, idempotent replay, bridge errors, and unknown-result behavior pass without real network calls。

- [ ] **Step 5: Commit**

```powershell
git add starlink-dimension-router/src
git commit -m "feat: add standalone core user API forwarding"
```

### Task 5: Add Core aggregate metrics, bridge configuration, and migration support

**Files:**
- Modify: `src-core/src/store.rs`
- Create: `src-core/src/admin_summary.rs`
- Modify: `src-core/src/lib.rs`
- Create: `starlink-dimension-router/src/admin_routes.rs`
- Create: `starlink-dimension-router/src/bridge_config.rs`
- Create: `starlink-dimension-router/src/migration.rs`
- Test: `src-core/src/admin_summary.rs`, `starlink-dimension-router/src/admin_routes.rs`, `starlink-dimension-router/src/migration.rs`

**Interfaces:**
- `CoreStore::admin_summary(now_ms) -> Result<CoreAdminSummary, CoreError>` returns active user-Key count, CORE total/available/held/settled balances, today’s settled amount, and running/queued/reconcile counts。
- `BridgeConfigStore::test_then_save(candidate) -> Result<BridgeStatusSnapshot, BridgeError>` never writes an unverified bridge secret。
- Admin routes implement the `/admin/v1` contract from the spec and use a Core administrator Principal, not the AI Work bridge secret。
- Test helpers created in Step 1: `store_with_user_key_and_settlement() -> CoreStore` creates one active key and one settled ledger event; `now_ms() -> i64` returns the fixed test timestamp `1_758_000_000_000`; `unreachable_candidate() -> BridgeConfigCandidate` points to a local closed port and contains a fake secret。

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn admin_summary_separates_core_ledger_from_upstream_snapshot() {
    let summary = store_with_user_key_and_settlement().admin_summary(now_ms()).unwrap();
    assert_eq!(summary.active_api_keys, 1);
    assert_eq!(summary.core_settled_today, 7);
    assert_eq!(summary.upstream_credits, None);
}

#[test]
fn failed_bridge_test_does_not_replace_last_good_config() {
    let store = BridgeConfigStore::with_verified("https://good.example", "digest");
    assert!(store.test_then_save(unreachable_candidate()).is_err());
    assert_eq!(store.base_url(), "https://good.example");
}
```

- [ ] **Step 2: Run tests and verify RED**

Run the `src-core` tests and standalone admin tests with the D-drive target. Expected: FAIL because aggregate queries, bridge-config validation, and admin routes do not exist。

- [ ] **Step 3: Implement read-only aggregate projections and admin writes**

Add SQL-backed aggregate queries that never return per-account upstream rows. Include separate fields for `core_total`, `core_available`, `core_held`, `core_settled`, `core_settled_today`, and an optional `upstream_snapshot` with source/timestamp/freshness. Add users, ordinary Key issue/revoke, scope and quota operations using existing `CoreStore` authorization helpers. Store the bridge Base URL and secret reference only after `BridgeClient::test()` succeeds. The summary endpoint must return stale snapshots with `fresh: false` rather than fabricating zeros.

- [ ] **Step 4: Implement read-only migration and apply safeguards**

Read the current AI Work/Core data root without deleting it. Generate source hashes, counts and unmapped records; require an explicit mapping and administrator confirmation before applying. Copy the Core SQLite database or import rows into the new data directory only when the target is empty or the migration ID is idempotently recognized. Preserve old JSON, SQLite, assets and video output on every error.

- [ ] **Step 5: Run tests and commit**

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --offline --locked
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
git add src-core/src starlink-dimension-router/src
git commit -m "feat: add core metrics bridge config and migration"
```

### Task 6: Build the independent “星链维度分流系统” management UI

**Files:**
- Create: `starlink-dimension-router-ui/package.json`
- Create: `starlink-dimension-router-ui/index.html`
- Create: `starlink-dimension-router-ui/src/main.tsx`
- Create: `starlink-dimension-router-ui/src/App.tsx`
- Create: `starlink-dimension-router-ui/src/api.ts`
- Create: `starlink-dimension-router-ui/src/types.ts`
- Create: `starlink-dimension-router-ui/src/components/BridgePanel.tsx`
- Create: `starlink-dimension-router-ui/src/components/SummaryCards.tsx`
- Create: `starlink-dimension-router-ui/src/components/UsersKeysPanel.tsx`
- Create: `starlink-dimension-router-ui/src/components/QuotaPanel.tsx`
- Create: `starlink-dimension-router-ui/src/components/JobsPanel.tsx`
- Test: `starlink-dimension-router-ui/src/components/*.test.tsx`
- Modify: `src-tauri/src/api_server/` only when shared DTO serialization is required; do not reintroduce CORE management into AI Work UI。

**Interfaces:**
- UI API client uses only `/admin/v1/*` and stores the Core admin session in memory。
- `SummaryCards` accepts `CoreAdminSummary` and renders no account-pool fields。
- `BridgePanel` calls test before save and exposes only masked bridge state。
- Test helpers created in Step 1: `fixtureSummary() -> CoreAdminSummary` contains one active key and separate CORE balances; `failingBridgeApi()` implements `test()` as a rejected promise and tracks `save()` calls; `api` in the test is the same object passed to `BridgePanel`。

- [ ] **Step 1: Write the failing component tests**

```tsx
it('shows active keys and separate credit aggregates without pool data', () => {
  render(<SummaryCards summary={fixtureSummary()} />);
  expect(screen.getByText('活跃 API Key')).toBeInTheDocument();
  expect(screen.getByText('今日消耗积分')).toBeInTheDocument();
  expect(screen.queryByText(/账号池|UID|Cookie|JWT/)).not.toBeInTheDocument();
});

it('does not save bridge settings when the connection test fails', async () => {
  render(<BridgePanel api={failingBridgeApi()} />);
  await user.click(screen.getByRole('button', { name: '测试并保存' }));
  expect(screen.getByText('连接失败')).toBeInTheDocument();
  expect(api.save).not.toHaveBeenCalled();
});
```

- [ ] **Step 2: Run tests and verify RED**

Run the standalone UI test command. Expected: FAIL because the new independent app and panels do not exist。

- [ ] **Step 3: Implement the UI and standalone desktop shell**

Reuse the visual language of the current Core workspace but remove Tauri `invoke` calls and account-pool controls. The first screen is the connection test and summary dashboard. Add separate cards for active ordinary keys, CORE available/held/settled/today settled, queued/running/reconcile jobs, and optional upstream aggregate with source/time. Add bridge configuration, users/keys, quota ledger, jobs and migration sections. Mask all secrets; show a newly issued ordinary Key once. Build the UI as a standalone Tauri shell under `starlink-dimension-router-ui/src-tauri`, with the HTTP API as the only backend integration.

- [ ] **Step 4: Run UI tests and static checks**

Run:

```powershell
Push-Location starlink-dimension-router-ui
npm.cmd test -- --run
npx.cmd tsc --noEmit
npm.cmd run build
Pop-Location
```

Expected: all component tests, TypeScript checks and production build pass。

- [ ] **Step 5: Commit**

```powershell
git add starlink-dimension-router-ui
git commit -m "feat: add standalone starlink router console"
```

### Task 7: Migration, packaging and bridge-only rollout

**Files:**
- Create: `scripts/build-starlink-router.ps1`
- Create: `scripts/migrate-starlink-router.ps1`
- Modify: `docs/server-deployment.md`
- Modify: `docs/user-manual.md`
- Modify: `docs/mcp-seedance.md`
- Test: `scripts/test-starlink-router-migration.mjs`

**Interfaces:**
- Build output: `D:\gpt\starlink-dimension-router-release\release\starlink-dimension-router.exe`。
- Migration output: a read-only report with source hashes, counts, unmapped records and explicit apply result。
- Deployment docs describe AI Work 7864 as execution endpoint and Starlink Router 7865 as the public endpoint。
- Test helpers created in Step 1: `runMigrationInspect(dataDir) -> Promise<MigrationReport>` invokes the script with inspect-only arguments; `fixtureDataDir` is a D-drive temporary directory containing sanitized legacy JSON fixtures and no credentials。

- [ ] **Step 1: Write migration/packaging tests**

```js
test('migration report never includes credentials or pool rows', async () => {
  const report = await runMigrationInspect(fixtureDataDir);
  expect(JSON.stringify(report)).not.toMatch(/cookie|jwt|uid|api_key_value/i);
  expect(report.unmapped_keys).toEqual([]);
});

test('build script uses D drive and produces the named executable', async () => {
  expect(buildScript).toContain('D:\\gpt');
  expect(buildScript).toContain('starlink-dimension-router.exe');
});
```

- [ ] **Step 2: Run tests and verify RED**

Run the migration test. Expected: FAIL because the standalone build and migration scripts do not exist。

- [ ] **Step 3: Implement migration and release build**

Make the migration script perform inspect first, print a summary, and require an explicit `-Apply` flag plus a migration ID before writing. Make the build script set `CARGO_TARGET_DIR`, `TEMP`, and `TMP` under `D:\gpt`, build the standalone API/UI release without bundling real upstream calls, verify the executable path and SHA-256, and leave the existing AI Work release untouched. Add deployment instructions for reverse proxy, HTTPS, firewall, health checks and separate ports.

- [ ] **Step 4: Run migration, build and release smoke checks**

Run:

```powershell
node scripts/test-starlink-router-migration.mjs
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
& '.\scripts\build-starlink-router.ps1' -OutputRoot 'D:\gpt\starlink-dimension-router-release'
Get-Item 'D:\gpt\starlink-dimension-router-release\release\starlink-dimension-router.exe'
```

Expected: inspect is read-only, the release exists, and no real upstream request is issued。

- [ ] **Step 5: Commit**

```powershell
git add scripts/build-starlink-router.ps1 scripts/migrate-starlink-router.ps1 scripts/test-starlink-router-migration.mjs docs/server-deployment.md docs/user-manual.md docs/mcp-seedance.md
git commit -m "feat: package and document starlink router rollout"
```

### Task 8: End-to-end verification and cleanup

**Files:**
- Modify: `.superpowers/sdd/2026-09-21-starlink-dimension-router/progress.md`
- Test: all Rust and UI test suites, bridge contract tests, migration smoke test

- [ ] **Step 1: Start only the fake bridge and standalone router**

Use a fake AI Work bridge bound to a D-drive test port. Verify `/health`, `/v1/models`, `/admin/v1/summary`, `/admin/v1/bridge/test`, normal text, Seedance video submission, status ownership, quota failure and idempotent replay. Do not use the production URL, real API Key or real video generation.

- [ ] **Step 2: Run complete verification**

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\starlink-router-cargo-target'
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --offline --locked
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-tauri/Cargo.toml --offline --locked
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
npm.cmd test -- --run
npm.cmd run test:buttons
npx.cmd tsc --noEmit
npm.cmd run build
node scripts/test-starlink-router-migration.mjs
git diff --check
```

Record existing unrelated failures by exact test name; do not label them as passing or silently repair unrelated worktree changes。

- [ ] **Step 3: Perform manual read-only UI acceptance**

Open the independent `starlink-dimension-router.exe`, connect it to the fake bridge, confirm the dashboard cards and sections, and verify no account-pool fields are rendered. Keep the existing AI Work release separate until the user explicitly switches production routing。

- [ ] **Step 4: Clean only test artifacts**

After the test processes exit, use `cargo clean` for the D-drive target and remove only the named temporary fake-bridge/test directories. Preserve the standalone release directory, migration backup, user data, and the current AI Work release. Confirm no project test directories remain under `C:\Users\StarLink\AppData\Local\Temp`.

- [ ] **Step 5: Update the ledger and final handoff**

Record commits, test counts, known pre-existing failures, release path, bridge configuration steps, and the fact that no real upstream request was sent. Mark the plan complete only after the standalone executable and manual UI acceptance are both verified。

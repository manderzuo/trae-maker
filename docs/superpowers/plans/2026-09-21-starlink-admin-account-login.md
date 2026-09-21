# 星链维度分流系统管理员账户登录 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 Core 管理界面的管理员 API Key 登录改为 `admin` 账户登录和内存会话，同时保留普通用户 API Key、积分、并发控制和 AI Work 桥接行为。

**Architecture:** 在 `src-core` 的 SQLite 账本中新增管理员凭证表，只保存 PBKDF2-HMAC-SHA256 哈希、随机盐和改密标记。独立 Router 进程在内存中管理高熵会话令牌，浏览器通过 HttpOnly/SameSite Cookie 访问管理 API；旧 Bearer 管理 Key 暂时保留为内部兼容通道，但从页面移除。初始密码只通过运行时环境 `STARLINK_ADMIN_INITIAL_PASSWORD` 提供，首次登录后强制修改，不写入仓库。

**Tech Stack:** Rust 2021、Axum 0.7、SQLite/rusqlite、PBKDF2-HMAC-SHA256、Tokio、原生 HTML/CSS/JavaScript、Node.js UI contract test。

**Spec:** `docs/superpowers/specs/2026-09-21-starlink-admin-account-login-design.md`

## Global Constraints

- 管理员账户默认为 `admin`，初始密码只由运行时环境提供；密码不得写入仓库、日志、响应或明文配置。
- 浏览器管理会话使用 HttpOnly、SameSite=Strict Cookie；公网 HTTPS 时必须设置 Secure。
- 普通用户 API Key、用户额度、Key 作用域、Key 最大并发数和 AI Work 桥接 Key 行为不得改变。
- 旧 Core 管理员 Bearer Key 仅作为兼容认证保留，不在管理页面展示，也不再由页面生成或保存到 localStorage。
- 生产和测试产生的 Cargo target、临时文件、日志统一放在 `D:\gpt`，不在 C 盘进行持续读写测试。
- 公网只允许 HTTPS 反向代理访问 `/admin` 和 `/v1/*`，不得直接暴露 Core 7865 端口。

## Review Focus

- 未登录或过期会话访问任一 `/admin/v1/*` 管理写接口时必须返回 401，不能落入旧的匿名路径。
- 错误账户和错误密码必须返回相同认证错误，不得通过响应内容枚举管理员账户。
- 初始密码来自环境变量且首次登录必须改密；环境变量缺失时不能静默创建弱默认密码。
- logout、改密和服务重启后旧会话必须失效，不能继续调用管理 API。
- 管理页面刷新和浏览器开发者工具中不能出现 Core 管理员 Key 输入框、localStorage 凭证或响应中的明文管理员 Key。

---

### Task 1: Core 管理员凭证持久化

**Files:**
- Create: `src-core/src/admin_credentials.rs`
- Create: `src-core/tests/admin_credentials.rs`
- Modify: `src-core/src/schema.rs` — 增加 `SCHEMA_V14`
- Modify: `src-core/src/store.rs` — schema version 14、迁移和凭证 CRUD
- Modify: `src-core/src/lib.rs` — 导出凭证 DTO
- Modify: `src-core/tests/assets_schema.rs`, `src-core/tests/jobs.rs`, `src-core/tests/migration.rs`, `src-core/tests/schema_bootstrap.rs` — 更新版本 fixture

**Interfaces:**
- Produces `AdminCredentialRecord { user_id: String, username: String, password_hash: String, salt: String, iterations: u32, must_change_password: bool }`。
- Produces `NewAdminCredential { user_id: String, username: String, password_hash: String, salt: String, iterations: u32, must_change_password: bool }`。
- Produces `CoreStore::find_admin_credential(&self, username: &str) -> Result<Option<AdminCredentialRecord>, CoreError>`。
- Produces `CoreStore::upsert_admin_credential(&self, input: NewAdminCredential) -> Result<(), CoreError>`。
- Produces `CoreStore::mark_admin_password_changed(&self, username: &str, password_hash: String, salt: String, iterations: u32) -> Result<(), CoreError>`。

- [ ] **Step 1: Write the failing persistence test**

Add `src-core/tests/admin_credentials.rs` with these cases:

```rust
#[test]
fn credential_round_trip_does_not_store_plaintext() {
    let dir = test_dir("admin-credential-round-trip");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_bootstrap_admin(NewUser { id: "admin".into(), name: "系统管理员".into(), role: UserRole::Admin }, "test").unwrap();
    store.upsert_admin_credential(NewAdminCredential {
        user_id: "admin".into(), username: "admin".into(), password_hash: "hash".into(),
        salt: "salt".into(), iterations: 600_000, must_change_password: true,
    }).unwrap();
    let saved = store.find_admin_credential("admin").unwrap().unwrap();
    assert_eq!(saved.user_id, "admin");
    assert_eq!(saved.password_hash, "hash");
    assert!(store.find_admin_credential("zuo123").unwrap().is_none());
}

#[test]
fn schema_v14_migrates_existing_v13_without_touching_users_or_keys() {
    let dir = test_dir("admin-credential-v13-migration");
    create_v13_fixture_with_admin_and_key(&dir);
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 14);
    assert_eq!(store.count_rows("users").unwrap(), 1);
    assert_eq!(store.count_rows("api_keys").unwrap(), 1);
    assert_eq!(store.table_count("admin_credentials").unwrap(), 1);
}
```

The second test must create a v13 database fixture, insert an admin user and API Key, run `migrate`, then assert schema 14 and the original user/key remain unchanged.

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --test admin_credentials --offline
```

Expected: FAIL because the v14 table and the `CoreStore` credential methods do not exist.

- [ ] **Step 3: Implement the minimal schema and store API**

Add `SCHEMA_V14`:

```sql
CREATE TABLE admin_credentials (
  user_id TEXT PRIMARY KEY REFERENCES users(id),
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  salt TEXT NOT NULL,
  iterations INTEGER NOT NULL CHECK(iterations >= 100000),
  must_change_password INTEGER NOT NULL CHECK(must_change_password IN (0,1)),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
```

Set `CURRENT_SCHEMA_VERSION` to 14, add the v13→v14 migration and keep all old fixture paths valid. Validate non-empty username/hash/salt, positive user existence, and map the SQLite boolean to `bool` in the public DTO. The migration must be idempotent for an already-created `admin_credentials` table.

- [ ] **Step 4: Run the focused and migration tests**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --test admin_credentials --offline
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --test migration --test schema_bootstrap --offline
```

Expected: all tests PASS and schema version reports 14.

- [ ] **Step 5: Commit the Core credential storage change**

```powershell
git add src-core/src/admin_credentials.rs src-core/src/schema.rs src-core/src/store.rs src-core/src/lib.rs src-core/tests/admin_credentials.rs src-core/tests/assets_schema.rs src-core/tests/jobs.rs src-core/tests/migration.rs src-core/tests/schema_bootstrap.rs
git commit -m "feat: add core admin credential storage"
```

### Task 2: Password hashing and in-memory admin sessions

**Files:**
- Create: `starlink-dimension-router/src/admin_session.rs`
- Modify: `starlink-dimension-router/Cargo.toml` and `starlink-dimension-router/Cargo.lock` — add `pbkdf2 = 0.12.2`, `hmac = 0.12.1`
- Modify: `starlink-dimension-router/src/state.rs` — session and login throttle state
- Modify: `starlink-dimension-router/src/lib.rs` — export module
- Modify: `starlink-dimension-router/src/main.rs` — initialize an absent admin credential from runtime password

**Interfaces:**
- Produces `AdminSessionStore::issue(user_id: String) -> (String, AdminSession)`.
- Produces `AdminSessionStore::lookup(token: &str, now_ms: i64) -> Option<AdminSession>`.
- Produces `AdminSessionStore::revoke(token: &str)` and `AdminSessionStore::revoke_user(user_id: &str)`.
- Produces `hash_password(password: &str) -> Result<PasswordHash, AuthSetupError>` and `verify_password(password: &str, record: &AdminCredentialRecord) -> bool`.
- Produces `ensure_initial_admin_credential(store: &CoreStore, initial_password: Option<&str>) -> Result<(), AuthSetupError>`; it only creates a credential when the `admin` Core user exists and no `admin` credential exists.

- [ ] **Step 1: Write failing password/session tests**

In `admin_session.rs`, add tests for password round-trip, wrong-password rejection, token lookup, token expiry, revoke, and per-user session revocation. Use a fixed test timestamp and assert that two hashes of the same password have different salts.

```rust
#[test]
fn password_hash_verifies_and_wrong_password_fails() {
    let first = hash_password("test-password").unwrap();
    let second = hash_password("test-password").unwrap();
    assert!(verify_password_value("test-password", &first));
    assert!(!verify_password_value("wrong-password", &first));
    assert_ne!(first.salt, second.salt);
    assert_ne!(first.hash, second.hash);
}

#[test]
fn sessions_expire_and_revoke() {
    let sessions = AdminSessionStore::new(100);
    let (token, session) = sessions.issue("admin".into(), 1_000);
    assert_eq!(sessions.lookup(&token, 1_050).unwrap().user_id, "admin");
    assert!(sessions.lookup(&token, session.expires_at_ms + 1).is_none());
    let (token2, _) = sessions.issue("admin".into(), 2_000);
    sessions.revoke(&token2);
    assert!(sessions.lookup(&token2, 2_001).is_none());
}
```

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml admin_session --offline
```

Expected: FAIL because the session store and password functions do not exist.

- [ ] **Step 3: Implement password hashing and session storage**

Use PBKDF2-HMAC-SHA256 with 600,000 iterations, a 16-byte random salt, a 32-byte derived key, and constant-time comparison. Encode salt/hash/token as URL-safe base64 without padding. Store only token hashes in the in-memory session map. Use a 12-hour expiry and `Mutex<HashMap<String, AdminSession>>`; expired entries are removed during lookup.

Add a login throttle map keyed by normalized remote address + username with five failures per 15-minute window. Successful login clears the counter. Do not include passwords, hashes, session tokens or authorization headers in `Debug` output or error messages.

At startup, read `STARLINK_ADMIN_INITIAL_PASSWORD` only if no credential exists. If an active `admin` Core user exists, hash that value and create `admin`/`must_change_password=true`; if no value is configured, do not create a weak implicit default and let the login page show initialization-required. The current local release test launch will set the environment variable outside the repository.

- [ ] **Step 4: Run the focused tests and Router unit tests**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml admin_session --offline
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
```

Expected: all password/session tests and the existing Router tests PASS.

- [ ] **Step 5: Commit session primitives**

```powershell
git add starlink-dimension-router/Cargo.toml starlink-dimension-router/Cargo.lock starlink-dimension-router/src/admin_session.rs starlink-dimension-router/src/state.rs starlink-dimension-router/src/lib.rs starlink-dimension-router/src/main.rs
git commit -m "feat: add admin password sessions"
```

### Task 3: Replace the protected Admin API authentication path

**Files:**
- Modify: `starlink-dimension-router/src/admin_auth.rs` — Cookie session extraction, Origin check, Bearer compatibility
- Modify: `starlink-dimension-router/src/admin_routes.rs` — login/session/logout/password handlers and bootstrap behavior
- Modify: `starlink-dimension-router/src/server.rs` — public auth routes and protected admin router layering
- Modify: `starlink-dimension-router/src/state.rs` — expose session/throttle state to handlers
- Create: `starlink-dimension-router/tests/admin_login.rs` — HTTP-level authentication tests

**Interfaces:**
- Adds `POST /admin/v1/login`, `GET /admin/v1/session`, `POST /admin/v1/logout`, `POST /admin/v1/password`.
- Protected admin middleware inserts the existing `aiwork_core::Principal` with a reserved `admin_session:<session-token-hash>` key id; Core authorization accepts this only for an authenticated admin session and still checks the active admin user.
- Existing Bearer admin Key requests remain accepted by the compatibility branch and continue to use `authorize_admin_principal`.

- [ ] **Step 1: Write failing HTTP tests**

Add tests using `tower::ServiceExt` and `axum::body::Body` for:

```rust
#[tokio::test]
async fn admin_login_sets_cookie_and_session_reads_summary() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    let session = get_with_cookie(&app, "/admin/v1/session", &cookie).await;
    assert_eq!(session.status(), StatusCode::OK);
    let summary = get_with_cookie(&app, "/admin/v1/summary", &cookie).await;
    assert_eq!(summary.status(), StatusCode::OK);
}

#[tokio::test]
async fn wrong_password_and_missing_session_are_unauthorized() {
    let app = test_app_with_admin_password("test-password");
    let wrong_password = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"wrong"})).await;
    let wrong_username = post_json(&app, "/admin/v1/login", json!({"username":"missing","password":"wrong"})).await;
    let missing_cookie = get(&app, "/admin/v1/summary").await;
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_username.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(missing_cookie.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response_error_type(wrong_password), response_error_type(wrong_username));
}

#[tokio::test]
async fn logout_and_password_change_invalidate_old_sessions() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    assert_eq!(post_with_cookie(&app, "/admin/v1/logout", &cookie, json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(get_with_cookie(&app, "/admin/v1/summary", &cookie).await.status(), StatusCode::UNAUTHORIZED);
    let login2 = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie2 = login2.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    assert_eq!(post_with_cookie(&app, "/admin/v1/password", &cookie2, json!({"current_password":"test-password","new_password":"new-test-password"})).await.status(), StatusCode::OK);
    assert_eq!(post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get_with_cookie(&app, "/admin/v1/summary", &cookie2).await.status(), StatusCode::UNAUTHORIZED);
}
```

The test fixture must create an admin Core user and credential hash without contacting AI Work. The protected request must prove that existing admin handlers still see an admin `Principal` and can read the summary.

- [ ] **Step 2: Run the HTTP tests and verify they fail**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --test admin_login --offline
```

Expected: FAIL because login routes and Cookie middleware do not exist.

- [ ] **Step 3: Implement routes and middleware**

Split `server::build_router` into:

```rust
let public_admin = Router::new()
    .route("/admin/v1/login", post(admin_routes::login))
    .route("/admin/v1/session", get(admin_routes::session))
    .route("/admin/v1/bootstrap", post(admin_routes::bootstrap));
let session_admin = admin_routes::router()
    .route("/admin/v1/logout", post(admin_routes::logout))
    .route("/admin/v1/password", post(admin_routes::change_password))
    .layer(from_fn_with_state(state.clone(), admin_auth::require_admin));
```

`require_admin` must accept a valid session Cookie first, check expiry and current admin role, then insert the existing `Principal`; otherwise it may fall back to the old Bearer path. For mutating session-authenticated requests, reject a present `Origin` that does not match the request Host with 403. Set `starlink_admin_session` with `Path=/admin`, `HttpOnly`, `SameSite=Strict`, and `Secure` whenever the request is HTTPS or `STARLINK_ROUTER_SECURE_COOKIES=true`.

Login responses must be generic on credential failure, must not return a Core Key, and must set `must_change_password`. Password changes require the current password, a new password of at least 10 characters, clear all old sessions, and issue a fresh session only after the hash is committed. Bootstrap must create or seed the account without issuing an API Key and must refuse to run once an admin credential exists.

- [ ] **Step 4: Run HTTP tests and the full Core/Router suites**

Run:

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --test admin_login --offline
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path src-core/Cargo.toml --offline
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path starlink-dimension-router/Cargo.toml --offline
```

Expected: all authentication, migration, concurrency and existing Router tests PASS.

- [ ] **Step 5: Commit the HTTP authentication migration**

```powershell
git add starlink-dimension-router/src/admin_auth.rs starlink-dimension-router/src/admin_routes.rs starlink-dimension-router/src/server.rs starlink-dimension-router/src/state.rs starlink-dimension-router/tests/admin_login.rs
git commit -m "feat: authenticate core admin with account sessions"
```

### Task 4: Replace the Core management page and deployment documentation

**Files:**
- Modify: `starlink-dimension-router/static/index.html` — login state, session fetch, logout, forced password change
- Modify: `scripts/test-starlink-router-ui.mjs` — UI contract assertions
- Modify: `docs/server-deployment.md` — account login, runtime initial password and HTTPS requirements
- Modify: `docs/user-manual.md` — update local/public admin instructions

**Interfaces:**
- Page boot calls `GET /admin/v1/session`; unauthenticated users see only the login card.
- All existing admin actions use `fetch` with same-origin credentials; no `Authorization` header is built by the page.
- On `must_change_password`, the dashboard stays hidden until password change succeeds.

- [ ] **Step 1: Write failing UI contract assertions**

Extend `scripts/test-starlink-router-ui.mjs` to assert that the HTML contains `admin/v1/login`, `admin/v1/session`, `admin/v1/logout`, a password-change control, `credentials:'same-origin'`, and does not contain `localStorage`, `Core 管理员 API Key（可选择保存到本机浏览器）`, or `ADMIN_KEY_STORAGE`.

- [ ] **Step 2: Run the UI contract and verify it fails**

Run:

```powershell
node scripts/test-starlink-router-ui.mjs
```

Expected: FAIL because the page still contains the Core Key input and localStorage code.

- [ ] **Step 3: Implement the account-login UI**

Replace the first panel with account/password login. On load, call `/admin/v1/session`; on 401 show the login panel; on success render the existing dashboard. Add logout and change-password controls. Preserve the 5-second summary refresh, Chinese scope checkboxes and maximum concurrency input. Do not display or persist any Core admin Key.

- [ ] **Step 4: Update docs and run UI/build verification**

Document:

```text
登录地址：https://api.gemstory.cn/admin
本地地址：http://127.0.0.1:7865/admin
首次启动前设置 STARLINK_ADMIN_INITIAL_PASSWORD
首次登录后必须修改初始密码
公网只经 HTTPS 反向代理，不暴露 7865
```

Run:

```powershell
node scripts/test-starlink-router-ui.mjs
git diff --check
```

Expected: UI contract PASS and no whitespace errors.

- [ ] **Step 5: Build, start and smoke-test the release on D:**

Use `scripts/build-starlink-router.ps1 -OutputRoot D:\gpt\starlink-dimension-router-release` with Cargo target/temp directories under `D:\gpt`. Stop only the current Core release process, set `STARLINK_ADMIN_INITIAL_PASSWORD` for the first startup without writing it to the repository, start the new release, and verify `/healthz`, `/admin`, login, session, logout and forced password change locally. Keep the existing AI Work process separate and do not send real upstream requests.

- [ ] **Step 6: Commit UI and deployment documentation**

```powershell
git add starlink-dimension-router/static/index.html scripts/test-starlink-router-ui.mjs docs/server-deployment.md docs/user-manual.md
git commit -m "feat: add core admin account login UI"
```

### Task 5: Final verification and handoff

**Files:**
- Modify: `.superpowers/sdd/2026-09-21-starlink-admin-account-login/progress.md`

- [ ] **Step 1: Run the complete offline verification**

Run the full `src-core` suite, full standalone Router suite, UI contract, and `git diff --check`. Redirect only long compiler output to `D:\gpt` and read the final summaries.

- [ ] **Step 2: Verify release artifacts**

Record SHA-256 for `D:\gpt\starlink-dimension-router-release\release\starlink-dimension-router.exe`, verify its process path, confirm `/healthz` is HTTP 200, and confirm the admin browser tab is open at `/admin` with the login page visible. Do not expose the initial password or any API Key in logs or the final response.

- [ ] **Step 3: Record the result and final review**

Append test commands, artifact hashes, and any rulings to the new progress ledger. Run a self-review against the spec's Review Focus, especially session invalidation and absence of localStorage credentials. If no subagent is used, record `Final review: self-review (no subagent tool)`.

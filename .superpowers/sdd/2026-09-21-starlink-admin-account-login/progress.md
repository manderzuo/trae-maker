# SDD ledger — plan: docs/superpowers/plans/2026-09-21-starlink-admin-account-login.md

## Setup

- Execution method: Native inline execution; no subagent.
- Workspace: existing shared working tree `E:\AIWORK\workspace\TraeWorkAssistant` retained because the previously approved Core concurrency and UI changes are uncommitted and must remain in scope.
- Global constraints read from the plan: password only from runtime environment, HttpOnly/SameSite session cookie, old Bearer admin key compatibility only, D:\gpt for build/temp, HTTPS reverse proxy for public access.
- Pre-flight shared interfaces: Task 1 produces Core credential persistence consumed by Task 2 hashing/initialization; Task 2 produces session state consumed by Task 3 middleware; Task 3 produces cookie login contract consumed by Task 4 UI; Task 4 produces the release consumed by Task 5 smoke verification.
- Ruling: use the current working tree instead of a new worktree — a new worktree would omit the user's existing uncommitted Core schema/UI changes that this plan must preserve; cost if wrong: unrelated local changes would not be present in the built release.

## Task 1: complete

- RED: `cargo test --manifest-path src-core/Cargo.toml --test admin_credentials --offline` failed because `NewAdminCredential` and the credential store methods were absent.
- GREEN: added schema v14 `admin_credentials`, credential DTOs and CoreStore methods; `admin_credentials` 2/2, `migration` 8/8, and `schema_bootstrap` 9/9 passed offline.
- Commit: `0a05c7b` (`feat: add core admin credential storage`).

## Task 2: complete

- RED: initial module inclusion exposed the expected missing session/hash symbols; the first implementation compile failed because `pbkdf2_hmac` requires `Sha256`, not `Hmac<Sha256>`.
- Ruling: use the crate's `pbkdf2_hmac::<Sha256>` API and retain constant-time comparison — cost if wrong: authentication hashes would not compile or verify.
- GREEN: password/session tests 2/2 and the full standalone Router suite 11/11 passed offline.
- Commit: `6207134` (`feat: add admin password sessions`).

## Task 3: complete

- RED: `cargo test --manifest-path starlink-dimension-router/Cargo.toml --test admin_login --offline` initially failed because the public login route and session cookie middleware were absent.
- GREEN: added login/session/logout/password routes, HttpOnly/SameSite session cookies, Origin validation for mutating browser requests, forced first-login password change, and retained Bearer admin-key compatibility. HTTP authentication tests 3/3 passed; the full Core and standalone Router suites passed offline.
- Ruling: extend Core admin authorization for the internal `admin_session:*` principal so authenticated browser sessions can reuse existing admin operations without creating a real API key. The branch still requires an active admin user; cost if wrong: only a compromised middleware-created principal could reach this path.
- Validation note: the installed Rust toolchain does not include `cargo-fmt`; formatting was not run because installing components would add unnecessary toolchain writes outside the D:\gpt test area.
- Commit: `1ce2367` (`feat: authenticate core admin with account sessions`).

## Task 4: complete

- RED: UI contract initially failed because the page still exposed the old administrator Key field and local storage flow.
- GREEN: replaced the page with account login, same-origin session credentials, forced password change, logout, and a dashboard hidden until authentication succeeds; retained summary refresh, bridge configuration, Chinese scopes and per-Key concurrency input. UI contract passed.
- Docs: updated local/public Core URLs, runtime initial-password setup, first-login password change and HTTPS reverse-proxy requirements.
- Commit: `7d21f09` (`feat: add core admin account login UI`).

## Task 5: complete

- Built the release with Cargo target/temp directories under `D:\gpt\starlink-admin-login-verify-target` / `D:\gpt`; no continuous test writes were made on C:.
- Release artifact: `D:\gpt\starlink-dimension-router-release\release\starlink-dimension-router.exe`.
- SHA-256: `4E37E9A7D2458923F5753F110ADC875ADBFC036E1CCBD2C130C0DECF248896E4`.
- Local smoke: `/healthz` HTTP 200; account login/session/forced-change/logout flow passed; admin page HTTP 200 and browser tab shows the new login page. Full Core suite, full Router suite, UI contract and `git diff --check` passed.
- The local acceptance credential was reset to the configured initial value with `must_change_password=true` after smoke testing, so the next manual login starts at the intended first-login screen.
- Self-review fix: bootstrap now rejects non-`admin` administrator names, and deployment/manual text no longer instructs operators to enter an admin Key in the page.
- Final review: self-review (no subagent tool).

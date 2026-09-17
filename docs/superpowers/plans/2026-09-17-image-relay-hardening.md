# Image Relay Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (recommended) or superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden image/video reference transport across local, LAN, and public deployments without changing the Seedance API contract.

**Architecture:** Keep the gateway as the only owner of temporary assets and introduce a small in-process limiter shared by asset/video handlers. Public liveness responses contain no pool details; the authenticated status endpoint remains the diagnostic source. The MCP bridge rejects unsupported local files before reading them into memory.

**Tech Stack:** Rust 2021, Axum 0.7, serde_json, Python 3 stdlib, PowerShell/Node smoke tests, Nginx/FRP deployment examples.

**Spec:** `docs/superpowers/specs/2026-09-17-image-relay-hardening-design.md`

## Global Constraints

- Do not send JWT, cookies, API keys, or authorization headers to logs or prompts.
- Do not scan directories; only explicitly supplied local media paths are eligible.
- Do not submit a real Seedance task during tests.
- Preserve `/v1/assets`, `/v1/videos/generations`, task polling, and API Key header compatibility.
- HTTPS is mandatory for public asset retrieval unless the explicit insecure-test switch is enabled.

---

### Task 1: Health response and deployment route hardening

**Files:**
- Modify: `src-tauri/src/api_server/auth.rs`
- Modify: `src-tauri/src/api_server/routes.rs`
- Modify: `deploy/frp/nginx.aiwork.conf.example`
- Modify: `src-python/tests/test_api_server.py`
- Test: Rust route/unit tests in `src-tauri/src/api_server/routes.rs` and `auth.rs`

- [x] Write tests proving `/health` has only liveness fields, `/healthz` is public and JSON, and pool/credit fields remain absent.
- [x] Run the focused Rust tests and observe the expected failure against the current detailed `/health` response.
- [x] Change `/health` to return `{status,running}` only; allow `/healthz` through the public-liveness auth exception while retaining its 200/503 decision.
- [x] Add exact `/healthz` proxying to the Nginx example before the general location.
- [x] Update the live API smoke test description/assertions so `/status` is the diagnostic endpoint.
- [x] Run focused tests, then the full Rust/Python test suites.

### Task 2: Temporary asset URL and transport-header hardening

**Files:**
- Modify: `src-tauri/src/api_server/assets.rs`
- Modify: `src-tauri/src/api_server/routes.rs`
- Modify: `docs/mcp-seedance.md`
- Modify: `docs/server-deployment.md`

- [x] Write tests for the default 30-minute TTL, HTTPS-only public base, explicit insecure opt-in, and response security headers.
- [x] Run the focused tests and observe failure for the old 2-hour/HTTP-allowed behavior.
- [x] Implement bounded `AIWORK_ASSET_TTL_SECS`, HTTPS enforcement with `AIWORK_ALLOW_INSECURE_ASSET_BASE=true`, and `no-store/no-referrer/nosniff` headers.
- [x] Keep token lookup constant-shape and ownership checks unchanged; do not add one-time consumption that could break Trae retries.
- [x] Update deployment docs to distinguish client HTTP gateway URLs from Trae-readable HTTPS asset URLs.
- [x] Run focused Rust tests and a no-credit local asset response check.

### Task 3: URL input validation

**Files:**
- Modify: `src-tauri/Cargo.toml` (add direct `url` dependency already present transitively)
- Modify: `src-tauri/src/api_server/video.rs`
- Test: `src-tauri/src/api_server/video.rs` unit tests

- [x] Write tests rejecting `file:`, `data:`, `ftp:`, missing-host, localhost, IPv4 private/reserved literals, and HTTP when insecure mode is off; accept HTTPS public URLs and explicit HTTP test mode.
- [x] Run focused tests and observe current acceptance of arbitrary strings.
- [x] Implement a URL parser helper using the `url` crate and conservative host/IP checks; apply it to both `image_urls` and `video_urls`.
- [x] Preserve asset-ID expansion and the existing ten-item limit.
- [x] Run focused Rust tests and full Rust tests.

### Task 4: Per-key resource limiting

**Files:**
- Create: `src-tauri/src/api_server/limits.rs`
- Modify: `src-tauri/src/api_server/mod.rs`
- Modify: `src-tauri/src/commands/api_server.rs`
- Modify: `src-tauri/src/api_server/routes.rs`
- Test: `src-tauri/src/api_server/limits.rs`

- [x] Write deterministic limiter tests for global concurrency, asset count/bytes windows, video submission windows, release on permit drop, and `Retry-After` metadata.
- [x] Run focused tests and observe failure because no limiter exists.
- [x] Implement a mutex-protected sliding-window limiter with environment-configured defaults: 32 concurrent requests, 30 asset uploads/minute/key, 256 MiB assets/hour/key, 3 video submissions/minute/key.
- [x] Construct one limiter per API server runtime and acquire permits after JSON/asset decoding validation but before asset creation/video task creation.
- [x] Return 429 JSON errors with `Retry-After`; ensure rejected requests cannot start a charged native task.
- [x] Run focused and full Rust tests; verify no real upstream call is made by rate-limit tests.

### Task 5: MCP local media preflight

**Files:**
- Modify: `src-python/seedance_mcp.py`
- Modify: `src-python/tests/test_seedance_mcp.py`

- [x] Write tests proving an unknown local file is rejected before `read_bytes`/upload and supported PNG/MP4 headers still reach the uploader.
- [x] Run the focused Python tests and observe failure because the current bridge accepts any readable file.
- [x] Implement a bounded header detector for PNG/JPEG/GIF/WebP/MP4/WebM, derive a matching MIME when available, and reject unsupported files before full read.
- [x] Keep explicit-path-only behavior and existing 32 MiB cap.
- [x] Run Python unit tests and Skill integration smoke tests.

### Task 6: Verification and operational documentation

**Files:**
- Modify: `docs/lan-frp-nginx-design.md`
- Modify: `deploy/frp/README.md`
- Modify: `docs/user-manual.md`
- Test: repository-wide test/build commands

- [x] Document the public health/status distinction, asset TTL/insecure switch, limits, and URL rules.
- [x] Run `cargo test --manifest-path src-tauri/Cargo.toml`, Python unit tests, `npm run test:skill`, `npm run build`, and the UI button scan.
- [x] Run read-only live checks for HTTPS `/health`, `/healthz`, unauthenticated `/v1/models`, and a fake asset URL; do not upload or generate real media.
- [x] Review `git diff`, confirm no secrets/runtime data changed, and report remaining deployment-only work (actual Nginx reload and key rotation).

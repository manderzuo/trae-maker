# Phase 3C 视频 jobs/attempts 实施计划

> 执行约束：所有 Cargo target、日志、SQLite fixture 和临时文件放 `D:\gpt`；每条命令先
> 设置 `TEMP/TMP=D:\gpt`，清空并断言 `AIWORK_*` 为空，使用 `--offline --locked`。不提交
> `src-core/Cargo.lock`、`target-fix/`、`data/`、`credentials/` 或用户已有视频/上传脏改动。

## Task 1：schema v9 与模型红测

**Files:** `src-core/src/schema.rs`, `src-core/src/models.rs`, `src-core/src/lib.rs`,
`src-core/src/store.rs`, `src-core/tests/jobs.rs`, `src-core/tests/schema_bootstrap.rs`

先写失败测试，覆盖：

- v8→v9 创建 `jobs`/`job_attempts` 和索引，保留 assets/request/lease/quota；
- legacy_jobs/video_tasks 不自动导入；
- owner、state、hash、lease/job/request/account 约束拒绝非法行；
- job 幂等键重放不重复创建 attempt。

实现 `JobState`、`JobAttemptState`、`CoreJob`、`CoreJobAttempt`、创建/查询输入和安全引用
校验；把 `CURRENT_SCHEMA_VERSION` 提升到 9，历史迁移分支全部经过 v8→v9。

验证：

```powershell
$env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
Get-ChildItem Env: | Where-Object Name -like 'AIWORK_*' | ForEach-Object { Remove-Item "Env:$($_.Name)" -ErrorAction SilentlyContinue }
if (Get-ChildItem Env: | Where-Object Name -like 'AIWORK_*') { throw 'AIWORK_* must be empty' }
cargo test --manifest-path src-core/Cargo.toml --target-dir D:\gpt\aiwork-phase3c-core-jobs --offline --locked --test jobs -- --nocapture
```

## Task 2：Core 原子 job/attempt 生命周期

**Files:** `src-core/src/jobs.rs` or `src-core/src/requests.rs`, `src-core/src/store.rs`,
`src-core/tests/jobs.rs`, `src-core/tests/recovery.rs`

抽取/复用现有 request+reservation+lease transaction，新增 `preflight_video_job`，把 job
和 attempt #1 写入同一事务。实现 owner-scoped query、cancel intent、一次性 settlement、
heartbeat、unknown recovery 和显式 retry guard。所有路径验证 lease/request/job/attempt
关联，拒绝跨用户、跨账号或错误 resource kind。

红测至少包含：

- success commit、explicit rejection release、confirmed cancel release；
- transport unknown/unsupported cancel 保持 unknown 与 hold；
- 重复 settlement、重复 cancel、重复 recovery 只有一次 ledger/audit；
- 重启时 queued 可重新排队，running/cancel_requested 无可信查询则 unknown；
- unknown job 不能被普通用户自动 retry。

## Task 3：Tauri 视频 adapter 与 Core enforce 路由

**Files:** `src-tauri/src/api_server/core_video.rs`, `core_bridge.rs`, `routes.rs`,
`src-tauri/src/api_server/mod.rs`, `src-tauri/src/api_server/phase3_video_smoke.rs`

新增仅接收 Core lease 的视频 adapter boundary 和 Mock adapter；扩展 CoreBridge 做视频
preflight/settlement。路由增加 Principal/scopes/idempotency 保护、Core job JSON 投影、
status/cancel/content owner 校验。缺 adapter 时必须在 preflight 前返回 501；不得触摸 legacy
pool 或 `video_tasks.json`。保留 off/shadow 原路由行为。

用 focused tests 覆盖：submit/replay/conflict、scope、伪造 user/account 字段、status
跨用户 404、cancel intent、Mock success/rejection/cancel/unknown、Core restart recovery、
no-adapter fail-closed。

## Task 4：D 盘回归、运维契约和审查

**Files:** `docs/credit-aware-scheduler-operations.md`, `AGENT.md`, `routes.rs` tests,
Phase 3C spec/plan

补充 schema v9、jobs/attempts、unknown/reconcile、取消和 legacy 回滚说明；运行 Core 全套、
Tauri focused/full、现有 Python/frontend 回归（不触达真实上游时）。检查 `git diff --check`、
staged 文件清单和用户未提交改动；提交目标：

```text
feat: persist video jobs and attempts in core
```

提交后立即核对 `git rev-parse HEAD` 与 `git rev-parse main`，保持目标 active；Phase 4
管理/部署仍未完成，不能标记总目标完成。

# AI Work Assistant Phase 3C：Core 视频 jobs/attempts 设计

日期：2026-09-20
状态：实施基线

## 1. 目标与边界

Phase 3C 把 `core_mode=enforce` 下的视频提交、查询、取消和恢复迁移到 Core 的持久化
`jobs`/`job_attempts`；用户额度、request、upstream lease 和审计继续由 Core 负责。现有
`core_mode=off`/`shadow` 的 legacy `video_tasks.json`、API Key owner 隔离和原生 Trae
SSE worker 保持兼容，不能与 Core 账本双写。

本阶段先以 Mock 视频 adapter 完成状态机和路由 contract tests。真实 Trae/WorkBuddy
视频协议、上游费用、取消接口和状态查询接口没有可靠契约前，Core enforce 必须返回
`501/scheduler_endpoint_not_enabled`，不得回退 legacy `ApiPool` 或宣称真实生成成功。

本阶段不实现对象存储、支付、真实上游计费换算、管理员 UI 或独立 Agent 进程编排；但
持久化模型必须为后续 Local Agent 和对账保留 opaque account/lease/request 引用。

## 2. 现状问题

- legacy 任务状态、幂等键和 owner 在进程内 map 与 `video_tasks.json`，Core 无法查询或恢复。
- Core request/lease 可以预占用户额度和账号并发，但没有 job 与 attempt 把异步视频生命周期
  连接到 lease；进程退出后无法区分排队、已发出和未知结果。
- 视频取消只能停本地可见性，不能记录“已请求但上游是否确认”这一不确定边界。
- `processing` 任务重启后不具备可审计的 retry/reconcile 判定；将其直接标记失败可能造成
  上游已接受而本地再次扣费/重试。

## 3. 数据模型（schema v9）

### 3.1 `jobs`

```text
id TEXT PRIMARY KEY
request_id TEXT NOT NULL UNIQUE REFERENCES requests(id)
user_id TEXT NOT NULL REFERENCES users(id)
kind TEXT NOT NULL CHECK(kind = 'video')
model TEXT NOT NULL
input_hash BLOB NOT NULL CHECK(length(input_hash) = 32)
state TEXT NOT NULL CHECK(state IN
  ('created','queued','running','cancel_requested','canceled','succeeded','failed','unknown'))
output_ref TEXT
artifact_ref TEXT
error_code TEXT
reconcile_required INTEGER NOT NULL CHECK(reconcile_required IN (0,1))
created_at_ms INTEGER NOT NULL
updated_at_ms INTEGER NOT NULL
last_heartbeat_ms INTEGER
cancel_requested_at_ms INTEGER
```

`input_hash` 是规范化视频请求体摘要；Core 不保存 prompt、参考图 token、JWT、Cookie 或
完整上游响应。`output_ref`/`artifact_ref` 只允许受限 opaque 引用，不能成为任意路径或
任意 URL 代理。`request_id` 唯一保证一个幂等提交最多产生一个 job。

### 3.2 `job_attempts`

```text
id TEXT PRIMARY KEY
job_id TEXT NOT NULL REFERENCES jobs(id)
attempt_no INTEGER NOT NULL CHECK(attempt_no > 0)
account_ref TEXT NOT NULL REFERENCES upstream_accounts(id)
lease_id TEXT NOT NULL UNIQUE REFERENCES upstream_leases(id)
upstream_request_ref TEXT
state TEXT NOT NULL CHECK(state IN
  ('queued','running','cancel_requested','canceled','succeeded','failed','unknown'))
error_code TEXT
retryable INTEGER NOT NULL CHECK(retryable IN (0,1))
created_at_ms INTEGER NOT NULL
updated_at_ms INTEGER NOT NULL
last_heartbeat_ms INTEGER
finished_at_ms INTEGER
UNIQUE(job_id, attempt_no)
```

Core 写入时必须验证 attempt 的 `job.user_id` 等于 Principal、`lease.request_id` 等于
`job.request_id`、`lease.account_ref` 等于 `attempt.account_ref`，且一个 lease 只能连接
一个 attempt。上游 request id 经过现有安全清洗；错误只保留固定 category。

### 3.3 迁移与 legacy 边界

v8→v9 在一个迁移事务中创建表、索引和约束，不导入 `video_tasks.json` 或 `legacy_jobs`。
legacy JSON 迁移在后续显式工具中进行；旧 `processing` 任务必须映射为
`unknown/reconcile_required`，不能静默当作成功或失败。重复迁移不会创建 active job。

## 4. 状态机与结算

```text
job:     created → queued → running → succeeded | failed
                              └→ cancel_requested → canceled | unknown
         queued ─────────────→ cancel_requested
         running ────────────→ unknown

attempt: queued → running → succeeded | failed
                         └→ cancel_requested → canceled | unknown
```

- `cancel_requested` 只代表 Core 已记录用户意图；只有 adapter 明确确认取消才进入
  `canceled` 并释放 quota hold。
- 上游已接受、超时、断连、进程退出、状态查询不可用或取消不确定都进入 `unknown`，保留
  reservation/lease 的对账语义，不自动 retry、不自动 release。
- 明确未接受的拒绝进入 `failed`，按现有 lease `Rejected { accepted: false }` 释放一次。
- 成功进入 `succeeded`，按保守成本策略 commit；没有真实 actual units 时保留 unknown
  actual，不把上游返回字段伪称为用户实际扣费。
- 所有 job/attempt 状态更新与对应 lease settlement 必须在同一 Core 事务完成，重复调用
  返回已应用/未应用，不重复写 ledger、health 或 audit。

## 5. Core API 契约

计划新增以下经过 owner 校验的 CoreStore 边界（具体 Rust 类型在实施计划中固定）：

- `preflight_video_job`: 在同一事务中创建 request、用户 reservation、upstream lease、
  job 和 attempt #1；相同幂等键返回原 job/lease，不重复占用。
- `video_job_for_user` / `video_job_for_request`: 只返回 Principal 所属 job，隐藏输入正文、
  account credentials 和其他用户字段。
- `request_video_cancel`: 只把 queued/running job 与 active attempt 置为
  `cancel_requested`，不提前 release；终态和他人 job 无状态/审计副作用。
- `settle_video_attempt`: 将 adapter 终态映射到 attempt/job/lease/request/quota/audit，
  严格一次性结算。
- `recover_video_jobs`: 启动时把 queued job 返回给本地调度队列；running 或
  cancel_requested 若没有可信状态查询则标记 unknown/reconcile_required，不自动重复调用。
- `retry_video_job`: 只允许管理员/对账流程对明确可 retry 的 failed job 建立新 attempt；不得
  对 unknown 自动重试，也不得复制用户 reservation。

## 6. Tauri 路由与 adapter 边界

Core enforce 路由必须：

1. `POST /v1/videos/generations` 要求 `Principal`、`videos:submit` 和客户端
   `Idempotency-Key`；请求体中的 `user_id`、account、provider、cost 字段全部忽略。
2. 先验证已注册的精确视频 adapter/account binding，再调用 Core preflight；没有 adapter
   时在创建 request/lease/job 前返回 501。
3. 返回 Core job 的稳定状态和 server request id；不返回 credentials、prompt 原文或内部
   存储路径。Core job 不再写 `video_tasks.json`。
4. `GET /v1/videos/{id}` 要求 `videos:read`，只能查询自己的 Core job；`content` 只有在
   artifact_ref 已验证且 owner 匹配时返回，不能把任意 `output_ref` 当 redirect。
5. `POST /v1/videos/{id}/cancel` 要求 `videos:cancel`，只记录 cancel intent 并由 adapter
   尝试取消；不确认时返回 job `unknown` 语义并保留对账状态。

Mock adapter 只在测试注入，覆盖 accepted/success、explicit rejection、cancel confirmed、
cancel unsupported、transport unknown、replay 和 restart recovery。真实 adapter 未完成时
生产启动注册表保持 fail-closed。

## 7. 安全、隔离和可观测性

- 所有查询按 `Principal.user_id` 过滤；API Key 只用于认证，不是 Core job owner。
- job/attempt/audit 只保存 hash、opaque id、受限 provider/resource label 和固定 error
  category；禁止 prompt、token、JWT、Cookie、完整 SSE 或原始上游错误。
- 每个状态变更记录 request/job/attempt/lease 的脱敏关联和 actor；恢复、取消、unknown、
  retry 都必须可审计。
- metrics 至少覆盖 job queued/running/unknown、cancel requested、recovery count、
  adapter rejection、idempotency replay/conflict 和 reconcile-required count。

## 8. 验收标准

1. schema v9 在 D 盘 fixture 中迁移通过，既有 v8 assets/request/lease/quota 数据不丢失，
   legacy video JSON 不被隐式导入。
2. Core tests 证明同一幂等键只创建一个 job/attempt，跨用户读取和取消失败且无副作用，
   duplicate settlement/recovery 不重复记账。
3. Tauri Mock route tests 证明 submit/status/cancel/replay/restart/unknown，且无 adapter
   时在任何 Core hold 之前稳定 501。
4. off/shadow legacy 视频回归不变；enforce 不回退 `ApiPool`，不写 `video_tasks.json`。
5. 所有测试使用 Mock、`--offline --locked` 和 `D:\gpt` target/log/fixture；这不证明真实
   上游视频协议、余额或计费规则。

# Phase 4E 持久视频队列运维边界

## 当前可交付能力

- Core v11 将视频任务、用户额度预占、执行尝试和上游 lease 持久化。
- Worker 启动或周期循环前应调用过期 lease recovery；恢复结果为 `unknown` 的任务保留额度占用。
- 管理员可在桌面端 **API 管理 → Core 管理 → 视频持久队列** 查看脱敏状态、租约和额度摘要。
- 管理投影不包含 prompt、输入摘要、request/lease ID、上游账号标识、凭据、结果路径或完整上游引用。

## unknown 与重启处理

1. 进程启动、worker 重新连接或心跳过期后，先执行 Core recovery。
2. `unknown` / `reconcile_required` 任务不能自动换账号重放，也不能按租约 TTL 自动退款。
3. 只有获得明确的上游终态和受限费用证据后，管理员或受信任 adapter 才能调用 reconcile；重复 reconcile 必须保持幂等。
4. 载荷缺失、DPAPI 解密失败、心跳失败和上游响应不确定都按 unknown 处理，不得误报成功。

当前版本的 `VideoQueueWorker` 是可测试的 worker 边界，生产启动生命周期和真实上游 adapter 尚未自动注册；没有可信 adapter 时，enforce 路径继续 fail-closed，返回 `501/scheduler_endpoint_not_enabled`。

## 用户自助额度查询边界

- `GET /v1/usage?limit=<n>` 只由 Core enforce 路径提供，并要求当前 API Key 的 Principal 具有 `usage:read`；默认 `limit=100`，范围为 1–100。用户只能看到自己的 `resource_kind` 余额投影和最近账本事件，不能通过参数读取其他用户。
- 响应的 `balances` 只包含 `available`、`held`、`settled`；有效 reserve 和 `unknown` 任务的保守占用继续留在 `held`，明确 commit 才进入 `settled`。`ledger` 仅保留资源、事件、金额、变化、可选 request id 和时间，不返回 prompt、凭据、上游账号或管理员内部信息。
- `core_mode=off/shadow`、未认证、缺 scope、limit 越界和 Core 存储错误分别保持 501/401/403/400/500 边界；查询是只读投影，不创建或释放额度，也不绕过 durable queue 的 recovery/reconcile 规则。
- 上游积分/余额仍是带来源和时间戳的 observation；上游余额不转换为用户额度，不把 legacy JSON 或真实上游 billing 当作用户账本事实。

## 回滚到 `core_mode=off`

1. 停止 API 服务和桌面管理操作，先备份整个 `<AIWORK_DATA_DIR>\data` 目录及 WAL/SHM 文件。
2. 将网关设置中的 `core_mode` 改为 `off` 并重启服务；这只切换新请求的 legacy 兼容路径，不删除 Core 数据库。
3. 不手工删除 jobs、reservations、leases 或 payload 文件；已有 unknown hold 仍需后续对账。
4. 恢复到 `shadow` / `enforce` 前，重新检查迁移报告、Key、scope、额度和 adapter 能力，不以 off 期间的 legacy 账本替代 Core 权威账本。

## 验证边界

本阶段使用离线 Mock 验证领取、心跳、缺失载荷、终态清理、管理员投影和用户隔离；不宣称真实视频协议、真实费用单位、真实余额、异步轮询、结果下载或公网部署已经验收。

## 双层预算 recovery/runbook（schema v12）

### 启动前检查

1. 停止 API 写入和 worker 新任务领取，确认 Core SQLite、WAL/SHM 与迁移报告均已备份。
2. 检查 `quota_budget_accounts` 的 Key 账户和可选 User cap 账户是否为 ready、版本有效、Key 仍归属当前用户且已启用。
3. 检查未完成迁移的 `legacy_unassigned`、held reservation、`unknown` 任务和最近的 `event_group_id`；任一映射缺失时维持 fail-closed。

### recovery 与 reconcile

1. worker 启动、重连或 lease 超时后，先恢复 durable queue，再按 `event_group_id` 对照 Key budget、User cap 和上游 lease。
2. 两层事件缺一、状态不一致、重复终态或上游结果不确定，统一标记 `reconcile_required`；`unknown` 保留 held，不按租约 TTL 自动退款。
3. 只有可信的上游终态和受限证据齐全时才允许幂等 reconcile。reconcile 不创建新的请求、不更换 Key 重放，也不把上游 observation 变成用户额度。
4. Key 预算未配置或迁移尚未 ready 时，用户用量接口返回 `key_quota_not_configured`；管理员先完成显式 legacy 分配，再恢复 enforce。

### 观测与回滚

运维界面和日志只显示脱敏的 Key prefix、scope、resource_kind、available、held、settled、预算版本、迁移状态和 reconcile 结果，不显示明文 Key、digest、prompt 或上游凭据。公网、LAN、本机都经过同一 Core；Nginx/FRP 不提供授权。

若需回滚，停止服务并恢复到 `core_mode=off`，保留 v12 数据、reservation、lease 和 unknown hold，禁止手工删除账本。恢复 `shadow`/`enforce` 前重新检查备份、迁移报告、Key、scope、版本和 adapter 能力。测试临时输出统一放在 `D:\gpt`。

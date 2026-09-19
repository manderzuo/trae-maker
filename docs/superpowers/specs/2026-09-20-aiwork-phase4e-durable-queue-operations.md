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

## 回滚到 `core_mode=off`

1. 停止 API 服务和桌面管理操作，先备份整个 `<AIWORK_DATA_DIR>\data` 目录及 WAL/SHM 文件。
2. 将网关设置中的 `core_mode` 改为 `off` 并重启服务；这只切换新请求的 legacy 兼容路径，不删除 Core 数据库。
3. 不手工删除 jobs、reservations、leases 或 payload 文件；已有 unknown hold 仍需后续对账。
4. 恢复到 `shadow` / `enforce` 前，重新检查迁移报告、Key、scope、额度和 adapter 能力，不以 off 期间的 legacy 账本替代 Core 权威账本。

## 验证边界

本阶段使用离线 Mock 验证领取、心跳、缺失载荷、终态清理、管理员投影和用户隔离；不宣称真实视频协议、真实费用单位、真实余额、异步轮询、结果下载或公网部署已经验收。

# Phase 4E 持久队列与重启对账实施计划

> 使用 `superpowers:executing-plans`：每个任务先写失败测试，再实现最小改动并在 D 盘验证。

**目标：** 在现有 Core v11 jobs/leases 基础上增加可证明的持久领取、用户公平性、心跳恢复和
排队取消边界，不猜测真实上游接口。

**设计：** `docs/superpowers/specs/2026-09-20-aiwork-phase4e-durable-queue-design.md`

## 约束

- 所有 Core 写入在 Immediate transaction 内完成；unknown/reconcile 不自动释放或换号重放。
- 不改变 legacy `off`/`shadow` 兼容路径；没有可信 production adapter 时 enforce 继续 fail-closed。
- 测试、fixture、Cargo target、日志、TEMP/TMP 只放 `D:\gpt`，Cargo 使用 `--offline --locked`，
  清除并断言 `AIWORK_*` 为空。
- 保留当前用户脏文件，不提交 `src-core/Cargo.lock`、`src-tauri/target-fix/`、`data/`、凭据或生成物。

## 任务

### Task 1：Core 队列模型与原子领取

- [x] 为 jobs 增加稳定队列序号/领取状态所需的最小 schema migration（不复制 prompt/body）。
- [x] 写入两用户 round-robin、同 job 单领取、资源类型隔离边界和重复提交测试。
- [x] 实现 `enqueue/claim` owner/状态/lease 校验，领取失败不得产生部分写入。

### Task 2：心跳、恢复、排队取消与显式对账边界

- [x] 先写 heartbeat owner/过期 claim、重启 recovery、queued cancel 和 unknown hold 测试；accepted
  cancel 仍由已有 lease 边界覆盖。
- [x] 复用现有 lease recovery/settlement，确保 job/attempt/request/reservation 终态一致，并清除
  已失效的 queue claim owner。
- [ ] 仅显式终态/受限证据允许 reconcile；重复 reconcile/release 幂等。

### Task 3：Tauri/worker 接入（Mock only）

- [ ] 增加最小后台 worker helper 和管理员安全投影，生产 adapter 缺失仍返回 501。
- [ ] 用 bounded Mock worker 验证领取、心跳、取消和重启，不启动真实服务或网络。
- [ ] 更新运维文档说明启动恢复、unknown hold 和回滚到 off。

### Task 4：验证与交付

- [ ] Core focused/full、Tauri focused/full、Vitest/build、diff check，全程 D 盘。
- [ ] 检查 no plaintext/digest/prompt/credential 泄露、用户隔离、资源隔离、幂等和无回退。
- [ ] 复核目标文件，报告 Phase 4E 证据与仍待真实上游契约的 Phase 4/5 项；不宣称总目标完成。

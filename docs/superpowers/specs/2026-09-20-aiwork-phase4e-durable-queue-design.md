# Phase 4E 持久队列、公平领取与重启对账设计

## 目标

在不猜测真实上游视频/图片协议的前提下，把 Core 已有的 `jobs`、`job_attempts`、request、
reservation 和 lease 组织成可恢复的持久队列边界：提交只产生一次排队事实，worker 只能在
同一事务中领取明确 job，进程重启或心跳过期进入对账态，不因不确定结果自动换号重试或退款。

这一阶段不注册真实上游 adapter，不改变 legacy `off`/`shadow` 路径，也不把 Mock 成功描述成
真实视频生成或真实计费成功。

## 当前事实

- Core v11 已持久化 `jobs`/`job_attempts` 以及队列调度约束。视频入队只建立用户额度
  hold、request、job 和脱敏调度元数据，不选择账号，也不创建 upstream lease。
- `claim_next_video_job`/`claim_video_job_by_id` 在同一 Immediate transaction 内完成公平领取、
  账号选择、lease/attempt 创建和 running 转换；`heartbeat_video_job` 只允许当前 worker
  续期同一 job、attempt 和 lease。
- `recover_expired_upstream_leases` 已把过期 held/active lease、request、video job 和 attempt
  统一转为 `unknown` 并保留 quota hold；需要把这条恢复逻辑暴露给队列 worker 的启动/心跳流程。
- 现有 Core scheduler 通过账号 observation、lease 和容量做安全选择；它不应被新的队列层
  绕过。队列只决定顺序，账号选择只发生在领取事务中。
- 真实 adapter 的 accepted、success、rejection、cancel 和费用证据尚未有可核验契约；本阶段
  只使用 MockVideoAdapter/fixture。Tauri 已有一个显式 worker 边界，但生产启动仍不会
  自动注册未知的真实 adapter。

## 设计边界

1. **队列事实单一**：以 request/job 的 `created_at_ms` 加稳定 ID 作为顺序，队列状态只能由
   Core 事务改变；不能由内存 `VecDeque` 作为权威来源。
2. **公平领取**：领取候选先按每用户最早未领取 job 做 round-robin，再以入队时间/ID 稳定排序；
   事务内二次检查 job、request、lease、用户状态和取消状态，避免两个 worker 领取同一 job。
3. **资源隔离**：`video_job`、未来的 `chat`/`image` 使用独立 resource kind 和容量计数；
   不把不同资源的额度、队列或 upstream lease 混在一起。
4. **不确定结果**：worker 心跳失败、进程重启、上游 accepted 后无终态、取消未确认，统一进入
   `unknown`/`reconcile_required=1`，保留 hold；恢复只能查询/对账，不能自动换账号重放。
5. **取消语义**：排队未领取的 job 可在 Core 内安全取消并释放对应 reservation；已领取或已
   accepted 的 job 只记录取消意图，adapter 明确确认后才释放 hold。
6. **敏感数据**：队列投影只返回 hash、受限 model/resource label、状态和时间；不持久化 prompt、
   完整 body、JWT、Cookie、credentials 或上游完整响应。
7. **载荷边界**：Core job 仍只持久化输入 hash；Tauri 以 job id 为键把 adapter body 写入
   Windows 当前用户 DPAPI 保护的独立载荷文件，文件不进入 Core、日志或管理投影。写入先于
   Core enqueue，终态后删除，unknown 保留以支持对账。worker 读取失败或找不到载荷时只进入
   unknown 并保留 hold；它不会把异常误报为成功，也不会自动换号重放。

## 计划接口

新增的 Core API（名称可在 TDD 后微调）应包含：

- `enqueue/claim`：原子创建/领取一条队列工作项，重复幂等键只能得到既有 job。
- `heartbeat`：只延长当前 lease/job attempt 的租期，owner、job、lease 不匹配即拒绝。
- `recover`：启动/worker 前调用已有 lease recovery；返回待对账安全投影。
- `cancel_queued`：只处理尚未领取的 job，不能把 running/accepted 误判为可退款。
- `reconcile`：显式输入受限终态/费用证据摘要后结算；没有证据不得释放 unknown hold。

所有写操作使用 `TransactionBehavior::Immediate`，并写脱敏 audit 事件。生产路由在没有可信
adapter 时继续在 preflight 前返回 `501/scheduler_endpoint_not_enabled`，不创建 request、
reservation、lease 或 job。

## 验证矩阵

- 两个用户交替领取时不会出现单用户长期霸占；同一 job 不会被两个 worker 领取。
- 不同 resource kind 的队列/额度互不透支；用户/Key 归属不匹配不能读取或领取。
- 重复提交只返回原 job；hash 冲突拒绝且不新增 reservation/job。
- 进程重启、租约过期、心跳失败、accepted 无终态、取消未确认都保留 unknown hold，不能自动重试。
- queued cancel 只释放一次；running/accepted cancel 只记录意图；确认取消才释放一次。
- Core 全套离线 Mock 回归继续通过；不运行真实网络、真实账号或真实账单测试。
- 新增的 job heartbeat/recovery 加上 Tauri Mock worker 已证明：持久载荷可在独立 worker
  中读取、领取、执行并结算；缺失载荷会保留 unknown hold。它仍不证明真实上游视频协议、
  费用单位或重启后对真实上游状态的自动查询。

## 未解决边界

Phase 4E 不证明真实上游队列、视频/图片协议、费用单位或退款规则。真实 adapter、启动时
worker 生命周期接线、异步轮询、结果下载、部署公网能力和 Skill/MCP Core job 契约需要在
获得可核验上游接口后单独验收。

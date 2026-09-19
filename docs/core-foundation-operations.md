# Core 基础运维手册（Phase 0/1）

本文只描述统一网关的 Core 身份、幂等、逻辑额度和迁移基础。Phase 1 的闭环使用
内存 Mock executor 验证，不代表真实上游余额、真实上游计费或真实视频/图片生成已经
验证。

## 1. 三种运行模式

网关设置中的 `core_mode` 只能是 `off`、`shadow` 或 `enforce`，默认是 `off`。

| 模式 | 身份与请求路径 | 额度行为 | 适用场景 |
| --- | --- | --- | --- |
| `off` | 继续使用现有 legacy 鉴权和请求路径；Core 不作为权威决策源 | 不由 Core 预占/结算 | 默认兼容、尚未完成迁移 |
| `shadow` | Core 打开并可观察 Key/用户匹配；legacy 仍负责放行 | 不用 Core 结果拒绝请求，也不把 shadow 观察当作扣费 | 迁移核对、生成 parity report |
| `enforce` | Core Key → Principal 是权威身份；用户、Key、scope、policy、幂等和额度失败即拒绝 | 先原子预占，再按成功/明确失败/不确定结果结算 | 完成迁移核对后的小范围启用 |

Phase 1 的 `enforce` 只覆盖非流式 Chat 闭环。`stream=true` 明确返回 501，不能借流式
路径绕过预算；视频、素材、取消、重启对账和真实账号调度属于后续独立计划。

## 2. Core 数据库和停机备份

默认数据根目录是 `%APPDATA%\\AIWorkAssistant`；设置 `AIWORK_DATA_DIR` 后，Core 数据库位于：

```text
<AIWORK_DATA_DIR>\\data\\core.sqlite3
```

例如桌面默认位置为 `%APPDATA%\\AIWorkAssistant\\data\\core.sqlite3`。数据库使用 SQLite
WAL、外键和事务迁移。备份采用停机文件级方式：

1. 停止 API 服务并退出桌面应用，确认没有其他进程打开 Core 数据库。
2. 复制整个 `data` 目录（至少包括 `core.sqlite3`，以及存在时的 `core.sqlite3-wal`
   和 `core.sqlite3-shm`）到带时间戳的只读备份目录。
3. 同时保留旧 JSON、日志和迁移报告；不要在备份过程中复制正在写入的 SQLite 文件。
4. 恢复前先停机，把备份恢复到独立数据根目录，启动后检查 `core_status` 的 schema、
   外键和余额，再切回生产目录。

旧 JSON 不会因为 Core 启用而删除或覆盖。它们仍是 legacy 兼容数据和迁移输入；
`remaining_credits.json` 等上游/本地缓存不会被当作 Core 用户额度或真实计费结果导入。

## 3. 管理员、Key 与逻辑额度

所有管理命令都要求传入真实的 `admin_api_key`，服务端从 Key 记录解析 Principal 并再次
校验 admin 角色；不能用 `actor_user_id` 或用户 ID 代替 Key。首个 admin 的 bootstrap
必须在受控初始化环境完成，后续使用正常管理 Key。

Key 的明文只在 `core_api_key_issue` 成功响应中显示一次。Core 只保存 digest 和 prefix，
不会把明文 Key 写入 SQLite、审计元数据或日志；应立即放入外部安全存储，遗失只能撤销后
重新签发。Key scope 至少应按实际能力授予，例如 Chat 需要 `chat:invoke`。

Core grant 是逻辑额度账本操作，不是向上游查询余额。管理员通过 `core_quota_grant`
指定 `user_id`、`resource_kind`、整数 `amount` 和非空 `reason`；常用 Chat 资源名为
`chat_request`。每次调整都记录 actor、原因、delta 和余额，不能把该余额解释为上游
账户剩余积分或已向上游支付的费用。

## 4. 迁移和启用顺序

推荐顺序如下：

1. 停机并完成上文的文件级备份，保留旧 JSON 原件。
2. 在 `off` 或 `shadow` 下运行 `core_migration_inspect`，检查用户、Key、素材、任务、
   observation 的 owner 映射、文件存在性、大小和 SHA-256；缺失或不确定项必须停在
   `legacy_unverified`/`reconcile_required`，不能猜测 owner。
3. 使用管理员 Key 执行 `core_migration_apply`。迁移在单个事务内写入记录；旧明文 Key
   不写入 Core，旧 Key 的 legacy 使用状态按迁移结果禁用/标记。
4. 保存 inspect/apply 输出和 parity report，逐项核对 legacy 数量、owner、资源状态和
   账本边界。迁移不会把旧 JSON 的 remaining credits 变成 Core grant。
5. 先在 `shadow` 观察鉴权和映射，再确认 **user、Key、scope、cost policy、grant、
   parity report** 齐全，才可小范围切换 `core_mode=enforce`。
6. 若出现未知上游结果，Core 保留 reservation 为 `unknown`，等待后续对账；不能直接
   当作未扣费并释放。

## 5. Phase 1 验证边界

批准的 smoke 入口为 `src-core/tests/full_phase1.rs` 中的 `run_phase1_smoke()`。它按固定
顺序验证 admin/user、一次性 Key、grant 2、成功 1、同幂等键重放、预算不足、timeout→
unknown、Store 重启恢复、审计和不超额余额。executor 只能是内存 Mock；测试不得读取
`AGENT_HOST`、`remaining_credits.json`、真实 API Key，也不得调用当前运行的服务。

允许作为 Phase 1 证据的本地验证包括：

```powershell
cargo test --offline --manifest-path src-core/Cargo.toml --test full_phase1 -- --nocapture
cargo test --offline --manifest-path src-core/Cargo.toml
```

不要把真实 upstream 测试、真实账号余额、真实计费响应或真实生成结果写进 Phase 1
验收结论；尤其不要把 `src-python/tests/test_api_server.py` 的真实上游路径作为本计划
smoke 证据。真实上游调度、credit-aware scheduler、媒体生命周期和生产部署必须在后续
独立计划中单独设计、审阅和验证。

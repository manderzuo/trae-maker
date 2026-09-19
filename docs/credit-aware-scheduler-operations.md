# Credit-Aware Scheduler 运维手册（Phase 2）

本文说明 Core schema v6、`scheduler_mode`、账号健康、观测刷新、lease 恢复、管理员
状态和回滚边界。它描述的是本地持久化与 Mock/fixture 验证结果，不等同于真实上游余额、
真实扣费或生产账号可用性证明。

## 1. 停机备份与数据边界

Core 数据库位于 `<AIWORK_DATA_DIR>\data\core.sqlite3`，默认根目录为
`%APPDATA%\AIWorkAssistant`。备份或恢复前必须停止 API 服务并退出桌面应用；复制整个
`data` 目录，至少保留 `core.sqlite3`，以及存在时的 `core.sqlite3-wal` 和
`core.sqlite3-shm`。不要在 SQLite 正在写入时只复制单个主库文件。

schema v6 的 `upstream_accounts`、`upstream_observations`、`upstream_leases` 与用户
`quota_*` 账本分离。`credentials_ref` 是不透明引用，不是 credential 内容。旧的
`remaining_credits.json`、WorkBuddy cache 和其他 JSON 只可作为迁移/诊断输入；同步时会
标记为 `json_cache`/stale，不能成为 enforce 的 fresh reader 结果，也不能给用户发 grant。

## 2. 模式和启用前检查

| `core_mode` | `scheduler_mode` | 行为 |
| --- | --- | --- |
| `off` | `off` | 继续使用 legacy `ApiPool`；Core 不作为请求决策源。 |
| `shadow` | `off`/`shadow` | 只同步/观察、生成 parity/诊断；不拒绝 legacy 请求、不预占用户额度。 |
| `enforce` | `enforce` | Core Principal、policy、grant、fresh observation、并发 lease 和结算都是权威条件。 |

启用 enforce 前，管理员必须确认：

1. Core 数据库已备份，admin API Key 可用，账号同步只写 opaque account/credential 引用。
2. 每个资源有 cost policy，用户拥有匹配 resource 的 grant，账号有明确 provider/region/
   capability 绑定和 `max_concurrency`。
3. reader 已产生 `source=reader`、状态 fresh 且未过期的观测；失败观测保留最后成功值，
   但当前记录为 failed/stale，不能伪装成零余额或 fresh。
4. executor 能明确返回 success、已接受/未接受 rejection 或 transport unknown。只要真实
   上游适配器尚未完成，启动/路由必须保持 fail-closed，不得退回 `ApiPool`。

启动同步不发起余额网络读取；reader refresh 必须是显式运维动作。shadow 的旧缓存 parity
只能帮助核对，不能改变 Core quota。

## 3. 健康状态和错误分类

健康转换只接受固定类别：`success`、`hard_credit`、`plan_limit`、`soft_rate`、
`not_found`、`session_dead`、`forbidden`、`server`、`transport_timeout`、
`transport_unknown`、`reader_failure`、`client`。Core 持久化 `state`、cooldown、
cooldown reason 和 consecutive error；未知输入会收敛到安全的 `client`，不会把原始适配器
错误文案传播到 API 或日志。

- `success` 恢复可用状态并清零连续错误；已被管理员禁用/标记 forbidden 的账号不会因一次
  reader 成功自动解禁。
- `hard_credit`/`plan_limit`/`soft_rate`/`server` 等进入 cooling，按固定策略退避；
  `session_dead` 禁用账号，`forbidden` 保持 forbidden。
- `reader_failure` 增加 reader failure 诊断并保留旧有效数值，不把失败写成余额为零。

## 4. 恢复和不确定结果

进程启动时只恢复持久化状态，不自动发起上游请求。若发现 lease 已过期且仍为
`held`/`active`：

1. upstream lease 变为 `unknown`，request/reservation 也变为 `unknown`；
2. hold 保留，**不产生 release ledger**，不自动释放并发槽，也不把它当作失败重试；
3. 后续只能由明确对账/人工流程决定最终结算。重复启动不会重复恢复同一条 lease。

这样可以避免“上游其实已经扣费、但本地因超时又释放并重试”的双扣/超额风险。

## 5. 状态、审计与隔离

管理员状态投影只允许 admin Principal，返回聚合字段：账号总数/启用数、fresh/stale
观测、active/unknown lease、reader failure、槽位饱和以及运行时 ready/error。HTTP enforce
`GET /status` 和桌面管理辅助路径都要求管理员；普通用户只能查询自己的授权业务结果，不能
读取管理员投影或其他用户的 quota/request 行。

结构化 scheduler JSONL 事件使用以下固定安全字段：`event`、时间、受限 request/lease
标识、`account_hash`、安全 provider/resource label、`observation_hash` 和固定
`error_category`。账号引用、观测 ID、凭据引用、JWT、Cookie、prompt、请求/响应 body
均不得原样写入结构化事件或 Core 审计元数据。旧 debug 请求日志若由诊断开关启用，仍须按
敏感日志对待。

## 6. 回滚

若 parity 或健康数据不可信，先停止 API 服务，保留 Core 数据库和日志备份，再把
`scheduler_mode`/`core_mode` 回退到 `off`。回退只恢复 legacy 业务路径，不会删除 Core
账本、unknown lease 或审计；不要把旧 JSON cache 转换成用户 grant，也不要手工删除
unknown lease 来“清空”状态。重新启用前应重新核对 admin Key、账号绑定、fresh reader、
policy、grant 和 executor。

## 7. 本地验证约定

测试不得访问真实网络、真实账号、`AGENT_HOST` 或真实运行服务。Cargo 的 target、日志和
临时 fixture 放在 `D:\gpt`；每个 Cargo 命令在同一个 PowerShell 进程中先清除并断言
`AIWORK_*` 环境变量为空，并使用 `--offline --locked`。例如：

```powershell
$aiworkEnv = Get-ChildItem Env: | Where-Object Name -like 'AIWORK_*'
$aiworkEnv | ForEach-Object { Remove-Item "Env:$($_.Name)" -ErrorAction SilentlyContinue }
if (Get-ChildItem Env: | Where-Object Name -like 'AIWORK_*') { throw 'AIWORK_* must be empty' }
cargo test --manifest-path src-core/Cargo.toml --target-dir D:/gpt/aiwork-task6-core --offline --locked --test recovery -- --nocapture 2>&1 | Tee-Object D:/gpt/aiwork-task6-core.log
```

不要运行 broad Tauri suite、真实上游 API 测试或把 `target-fix/`、`data/`、`credentials/`
和生成的 `Cargo.lock` 纳入提交。

# Credit-Aware Scheduler 运维手册（Phase 2–3B）

本文说明当前 Core schema v9（Phase 2 基线为 v8）、`scheduler_mode`、账号健康、观测刷新、lease 恢复、用户素材、
管理员状态和回滚边界。它描述的是本地持久化与 Mock/fixture 验证结果，不等同于真实上游
余额、真实扣费或生产账号可用性证明。

## 1. 停机备份与数据边界

Core 数据库位于 `<AIWORK_DATA_DIR>\data\core.sqlite3`，默认根目录为
`%APPDATA%\AIWorkAssistant`。备份或恢复前必须停止 API 服务并退出桌面应用；复制整个
`data` 目录，至少保留 `core.sqlite3`，以及存在时的 `core.sqlite3-wal` 和
`core.sqlite3-shm`。不要在 SQLite 正在写入时只复制单个主库文件。

当前 schema v9 的 `upstream_accounts`、`upstream_observations`、`upstream_leases`、用户
`quota_*` 账本、用户素材 `assets` 和视频 `jobs/job_attempts` 分离。`credentials_ref` 是不透明引用，不是 credential 内容。
schema v7→v8 只建立活动素材表，v8→v9 只建立视频 job/attempt 表，不把旧的
`assets.json` 或 `video_tasks.json` 导入 Core。旧的
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
`error_category`。账号引用、凭据引用、JWT、Cookie、prompt、请求/响应 body 均不得原样
写入结构化事件或 Core 审计元数据；通过 opaque 字符集和长度校验的 `observation_id` 可
保留用于审计追踪，不安全的观测标识仍改写为 hash。旧 debug 请求日志若由诊断开关启用，
仍须按敏感日志对待。

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

## 8. Phase 2 交付验证

`src-core/tests/full_phase2.rs` 是固定时间、固定 Mock 的集成 smoke：它注册不同
provider/region 的两个账号，验证 fresh/stale 观测、`max_concurrency=1` 的并发容量、
success/replay/rejection/transport-unknown、重启恢复、审计脱敏和 quota 不透支。
`src-tauri/src/api_server/phase2_smoke.rs` 只组合内存 Mock adapter；它不是生产上游
连接器，也不读取账号凭据、legacy `ApiPool` 或网络配置。

Phase 2 的 Mock smoke 仍不能证明真实 upstream 余额或账号可用性。Phase 3A 已在启动路径
注册显式的 `(account_ref, provider, credentials_ref)` Trae/WorkBuddy legacy adapter 绑定，
并要求对应 stream adapter readiness；缺少可信绑定时仍稳定返回
`501/scheduler_endpoint_not_enabled`，不会把 Mock smoke 的成功结果表述为真实 upstream
成功。交付时应同时保留 Core 全套离线回归、Tauri `phase2_smoke`/focused 回归和其 D 盘日志；
若 Python/frontend 测试会触达真实上游，则跳过并在报告中记录原因。

## 9. Phase 3A 流式启用与回滚

Phase 3A 的流式路径继续由 Core 负责 quota、request、upstream lease 和最终结算。
只有同时满足以下条件时，`core_mode=enforce` 的流式端点才可启用：

1. Core 已有 fresh observation、cost policy、user grant 和可用并发槽位。
2. 启动注册表为每个可选账号提供精确的
   `(account_ref, provider, credentials_ref)` 绑定，并注册对应 provider 的 stream adapter。
3. stream adapter 能给出终态 `success`、未接受的 `rejection`、确认 `canceled` 或
   `transport_unknown`；没有终态的流不得被当作成功。

缺少 stream adapter、账号绑定或 credentials locator 时，路由必须在 preflight 前返回
`501/scheduler_endpoint_not_enabled`，不创建 request、lease 或 quota hold，也绝不能
回退到 legacy `ApiPool`。OpenAI、Anthropic 和 Responses 流都必须发送各自的终止事件；
客户端断开、上游取消不支持或心跳失败时，结果保持 `unknown`，hold 不释放，等待对账。
确认取消或明确未接受的拒绝才释放 hold；重放同一幂等键不能再次调用上游。

交付证据全部是离线 Mock：`src-core/tests/full_phase3_streaming.rs` 覆盖账本和 lease
生命周期，`src-tauri/src/api_server/phase3_streaming_smoke.rs` 覆盖有界 SSE、幂等重放和
缺适配器 501，路由 focused tests 覆盖三种协议终止事件及客户端断开。测试 target、日志
和 fixture 固定放在 `D:\gpt`；这证明本地生命周期和边界，不证明真实上游账号可用性。

若绑定、reader 或健康数据不可信，先停止服务并备份 Core 数据库，再把
`core_mode`/`scheduler_mode` 回退到 `off`。回退恢复 legacy 业务路径，但不得删除
unknown lease、quota hold 或审计记录；重新启用前需重新核对账号绑定、credentials locator、
fresh observation、policy、grant 和 stream adapter readiness。

## 10. Phase 3B 用户素材归属与回滚

Phase 3B 在 `core_mode=enforce` 下把素材的用户归属、内容 token、过期状态和审计记录交给
Core `assets` 表；`user_id` 来自已认证 Principal，不能由 multipart 字段覆盖。素材上传只在
文件先以原子方式写入 `data/assets/<safe-storage-ref>`、再成功插入 Core 后返回；Core 插入
失败必须删除刚写入的文件，不能留下无主文件。`storage_ref` 只允许安全单级相对引用，读回时
还要校验 canonical path 位于素材根目录、文件大小和 SHA-256。

enforce 素材端点没有 legacy fallback：

1. `POST /v1/assets` 要求 Principal 和 `assets:write`，客户端提供的 `user_id` 只作为不可信
   输入而被忽略；重复 token、重复 id 或校验失败不得产生部分 Core 行。
2. `GET /v1/assets/{id}/content` 只接受匹配该素材、未过期的内容 token；错误 token、跨用户
   访问、过期记录、路径逃逸、大小或摘要不匹配统一返回安全的 not-found 语义。
3. 服务启动会把已过期的活动素材标记为 `expired` 并清理对应安全路径；清理失败不能删除
   Core 审计事实，也不能把记录重新变成 active。

`core_mode=off` 或 `shadow` 仍保留 legacy `assets.json` 兼容路径，但该路径不是 enforce 的
用户归属或安全证明。回退到 off 前应停止服务并备份 Core 与素材目录；回退不会把 legacy
索引导入 Core，也不会删除 Core 素材、审计或 quota 账本。

Phase 3B 的验证只使用 D 盘 fixture：Core `assets_schema` 覆盖 v7→v8 迁移、用户隔离、token
摘要、过期和原子约束；Tauri asset focused tests 覆盖安全文件存储/读回；route focused tests
覆盖 enforce 上传、owner 绑定、错误 token 和不回退 legacy。测试 target、日志和临时文件必须
放在 `D:\gpt`，Cargo 命令使用 `--offline --locked`，并在同一 PowerShell 进程清空并断言
`AIWORK_*` 环境变量为空。这些证据仍不证明真实上游账号、余额或计费规则。

## 11. Phase 3C 视频 jobs/attempts 启用与回滚

Phase 3C 在 `core_mode=enforce` 下把视频异步任务、尝试、request、reservation、upstream
lease 和 quota 结算链路交给 Core `jobs`/`job_attempts` 表。schema v8→v9 不导入
`video_tasks.json`；旧任务只在 `off`/`shadow` 的兼容路径存在，不是 enforce 的状态来源。

启用前必须同时满足：

1. Principal 具有 `videos:submit`，请求带严格 `Idempotency-Key`，Core 有 fresh observation、
   video policy、grant 和容量。
2. 启动注册表提供精确的 `(account_ref, provider, credentials_ref)` 绑定以及对应视频
   adapter；Core 负责选择 lease，adapter 不得自行选账号或读取持久化敏感数据。
3. adapter 能区分 accepted、成功、明确未接受的 rejection、确认 canceled 和
   transport unknown；accepted 后没有可信终态时必须保持 `unknown` 与 hold。

缺少 adapter、账号绑定、credentials locator 或 scheduler readiness 时，路由必须在
preflight 前返回 `501/scheduler_endpoint_not_enabled`，不创建 request、lease、quota hold、
job 或 attempt，也绝不能回退 legacy pool。相同幂等键只重放已有 job，不得再次调用上游。

视频 job 的查询要求 `videos:read` 并按 Principal owner 隔离；取消要求 `videos:cancel`，
先记录 `cancel_requested`，只有 adapter 明确确认取消才释放 hold。拒绝且明确未接受时释放
hold；网络超时、取消不支持、进程崩溃或重启恢复不确定时进入 `unknown`，保留 hold 并标记
reconcile。成功结果只保存受限 output/artifact 引用，内容读回仅允许受信任的 `video-store:`
引用和本地安全路径。

Phase 3C 的离线证据固定在 `D:\gpt`：Core 全套回归通过，Tauri focused 通过，Tauri 全量
`440 passed; 0 failed; 4 ignored`。这些测试覆盖 schema 迁移、owner/状态约束、幂等、成功、
拒绝、确认取消、不确定提交和缺 adapter fail-closed；它们不证明真实上游账号、余额、计费
规则或视频协议可用。回滚时停止服务并备份 Core 数据库，改回 `core_mode=off` 或 `shadow`；
不得删除 jobs、unknown lease、quota hold、审计事实或把它们伪造为成功。

## 12. Phase 4D Core 管理与验证

桌面端 **API 管理 → Core 管理** 是当前 Core 用户、Key 和逻辑额度的管理入口。它直接操作
`<AIWORK_DATA_DIR>\data\core.sqlite3`，不修改 legacy `api_keys.json` 的日限额和调度配置，
也不会把 legacy 上游余额转换为 Core grant。

启用管理前应准备真实 admin Core Key，并通过管理员身份认证；不能使用用户 Key、用户 ID 或
字面量 `admin`。页面只展示用户安全字段、Key prefix/scope/status 和额度聚合值，不展示 digest、
凭据或其他用户任务/素材；Key 明文只在签发结果中展示一次。关闭页面即丢弃管理员 Key，
不要把它复制进项目配置、日志或部署环境变量。

管理员可以创建用户、启用/禁用用户、签发/撤销 Key、按 `resource_kind` 发放和查询永久逻辑
额度。每种资源的 `available`、`held` 分开维护；禁用用户不删除 quota ledger、reservation、
jobs、attempts、assets 或 audit。最后一个活动管理员和当前登录管理员不能被禁用，重复撤销
不会重复扣额度或重复改变 Key 状态。

当前没有公网管理员 HTTP API、在线支付或真实上游账单同步。Core grant 只是本地逻辑授权；真实
上游余额、计费单位、视频协议和生产 adapter 在获得可核验契约前必须保持 fail-closed，不能
从 Mock、注释或 legacy JSON 推断成功。视频 adapter 未就绪时，enforce 视频提交应返回
`501/scheduler_endpoint_not_enabled`，且不创建 job、lease 或 quota hold。

Phase 4D 的本地验证仍只使用 Mock/fixture，且所有 Cargo target、日志、TEMP/TMP 和临时数据
放在 `D:\gpt`。推荐顺序：Core admin projection focused test、Tauri `core::tests::`、
Vitest、`npm run build`，最后再跑 Core/Tauri 全量回归。命令需使用 `--offline --locked`，
并在同一 PowerShell 进程清除和断言 `AIWORK_*` 环境变量为空；通过这些测试只能证明本地
身份、隔离、幂等和 fail-closed 边界，不能宣称真实上游或真实计费已验证。

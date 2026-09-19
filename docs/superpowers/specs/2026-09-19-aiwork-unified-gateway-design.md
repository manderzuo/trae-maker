# AI Work Assistant 统一网关、Local Agent 与多用户额度系统设计

日期：2026-09-19
状态：设计稿，待用户审阅
适用仓库：`E:\\AIWORK\\workspace\\TraeWorkAssistant`

## 1. 目标与边界

本设计把现有桌面端 API 网关扩展为一个可长期运行的本地工作服务：对外提供 OpenAI/Anthropic 兼容接口，对内通过 Local Agent 管理 Trae、WorkBuddy 及后续上游；支持多账号积分感知调度、多个用户的永久额度、异步任务恢复、审计和可观测性。

本设计不假设任何未被现有代码或上游响应证明的计费规则、余额接口或模型能力。所有“用户额度”均是管理员授予的内部逻辑单位；上游余额是带来源和时间戳的观测值，不能直接当作用户额度或现金余额。

明确不在第一阶段做的事情：支付、自动充值、对外售卖计费、猜测上游单价、绕过上游鉴权、把 DNS 子域名当作权限边界，以及为了兼容而伪造 embeddings 或其他未验证能力。

## 2. 已核验的现状

当前 Tauri/Rust 进程内置 Axum 网关，默认路由包括：

| 能力 | 当前路由 | 已核验行为 | 设计处理 |
|---|---|---|---|
| OpenAI Chat | `POST /v1/chat/completions` | 已有非流式、流式和双上游调度 | 保留协议，接入统一请求账本 |
| OpenAI Responses | `POST /v1/responses` | 已有 Responses 投影及部分 WB 工具编排 | 保留事件语义，统一任务状态 |
| Anthropic | `POST /v1/messages` | 已有 `x-api-key`/Bearer 和 SSE 投影 | 保留协议，绑定相同用户身份 |
| Legacy Completions | `POST /v1/completions` | 已有请求适配 | 保留，按 Chat 同一预算策略 |
| 模型目录 | `GET /v1/models` | 由 Trae/WB 目录聚合 | 只返回实际可用能力，不承诺额度 |
| Embeddings | `POST /v1/embeddings` | 明确返回 501 | 继续明确拒绝，直到有真实上游能力 |
| 图像 | `POST /v1/images/generations`、`/edits` | 已有 JSON 变体；不接受当前 multipart 变体 | 保留能力声明，错误时返回明确 415/501 |
| 素材 | `POST /v1/assets`、受控 content 路径 | 当前按 API Key 所有 | 改为用户所有，Key 只作为授权入口 |
| 视频 | `POST /v1/videos/generations`、状态/content | 已有异步任务和 Key 级幂等；重启不会恢复 processing worker | 迁移到持久化任务状态机与 Agent lease |
| 健康检查 | `/health`、`/healthz` | 免鉴权；业务路由要求 Key | 保留，但不泄露账号/额度详情 |

现状中的重要限制：API Key 文件含明文 key；API Key 只有每日请求计数，没有用户永久额度账本；`remaining_credits.json` 是缓存而非事务性余额；账号池没有持久化预占；`inflight` 是进程内计数；视频和素材以 `owner_key_id` 隔离；公网多个子域名最终进入同一核心网关，不能视为能力隔离。

## 3. 方案与部署拓扑

采用方案 B：**Local Core Service + Tauri Control Plane + Local Agent**。

### 3.1 组件职责

1. **`aiwork-core`**：独立的本地 Rust 服务/进程，拥有 SQLite、用户与 Key 鉴权、额度账本、请求幂等、预算预占、调度器、任务状态机、审计和管理 API。核心状态不依赖 WebView 生命周期。
2. **兼容网关层**：运行在 Core 内，保留现有 `/v1/*` 路由，将 OpenAI、Responses、Anthropic 和自定义异步接口转换为统一内部请求；只由它创建预算和上游 lease。
3. **Local Agent**：仅监听回环地址，负责调用真实上游、账号凭据、积分/能力刷新、视频轮询、素材上传和取消尝试。Agent 不接受公网业务请求，所有动作必须带 Core 签发的 lease。
4. **Tauri Control Plane**：负责桌面 UI、启动/停止 Core 和 Agent、管理账号凭据、用户/Key/额度、查看审计和运行状态。UI 不直接改 SQLite，也不把完整凭据返回给前端。
5. **Upstream Adapter**：Trae、WorkBuddy、Custom 和 Mock 各自实现统一适配器接口。Mock 是默认集成测试入口，不能读取真实凭据或消耗真实积分。

### 3.2 运行边界

- Core 与 Agent 默认只绑定 `127.0.0.1`；LAN/公网访问必须通过明确配置的反向代理/隧道进入 Core。
- 7864 作为兼容入口在迁移期保留，但最终由 Core 承接；旧嵌入式路由通过 feature flag 逐端点切换，不能出现两个组件同时写同一账本。
- Nginx、FRP 和子域名只负责传输与反向代理；权限、用户、能力和额度全部由 Core/API Key/scopes 决定。
- Core→Agent 使用本机随机安装密钥或受保护的本地凭据完成相互鉴权；Agent 不暴露可被公网直接访问的管理端点。

### 3.3 请求主链路

```text
客户端
  -> 兼容网关：解析协议/Key/幂等键/模型
  -> Core 事务：校验权限 + 估算 + 用户额度预占 + 幂等记录
  -> Scheduler：选择健康且积分观测足够新鲜的账号并建立 upstream lease
  -> Agent：调用真实或 Mock 上游
  -> Core：记录结果、结算/释放/标记 unknown、审计和指标
  -> 客户端：按原协议返回或继续轮询任务
```

预占用户额度成功但上游 lease 失败时必须释放用户预占；上游结果不确定时不能自动释放并假定没有扣费，而应进入 `unknown` 并由对账流程确认。

## 4. 身份、权限与安全不变量

### 4.1 身份规则

- API Key 只绑定一个 `user_id`；请求体、查询参数和普通自定义请求头中的 `user_id` 一律忽略或拒绝，不能覆盖鉴权结果。
- Key 只在创建响应中显示一次；数据库保存高熵 Key 的不可逆校验值、前缀和元数据，不保存明文。
- 现有明文 Key 不自动当作已确认用户身份。迁移必须由管理员提供 Key→用户映射；缺少映射的 Key 进入禁用/待认领状态并要求轮换。
- `/health` 和 `/healthz` 可以免鉴权，但不得包含 Key、用户、账号、精确积分或凭据。
- 资源所有权使用 `user_id`；API Key ID 仅用于审计和授权来源。撤销一个 Key 不应删除用户已有资源，除非管理员明确执行删除。

### 4.2 角色与 scope

内置角色为 `admin`、`operator`、`user`。Key 还必须带显式 scope，最小集合为：

| Scope | 作用 |
|---|---|
| `models:read` | 查看实际模型目录 |
| `chat:invoke` | Chat/Responses/Messages/Completions |
| `assets:write`、`assets:read` | 素材上传与读取 |
| `videos:submit`、`videos:read`、`videos:cancel` | 视频任务操作 |
| `usage:read` | 查看本人用量 |
| `admin:*` | 用户、额度、账号、策略、审计和迁移管理 |

任何管理端点都要求管理员用户或管理员 Key；业务响应只返回当前用户可见资源。日志和错误中禁止出现明文 Key、JWT、Cookie、完整 URL token、原始请求体和完整上游响应。

## 5. 数据模型

SQLite 是迁移完成后的唯一权威写入源。所有时间使用 UTC 的整数毫秒或带时区 RFC3339；金额/额度/计数使用整数，不使用浮点。

### 5.1 核心表

| 表 | 关键字段与约束 | 作用 |
|---|---|---|
| `users` | `id`、`name`、`role`、`status`、时间字段；name 可重复但 id 不可变 | 用户和权限主体 |
| `api_keys` | `id`、`user_id`、`key_mac` 唯一、`prefix`、scopes、状态、撤销时间 | Key 校验和授权 |
| `quota_policies` | `user_id`、`resource_kind`、上限/是否启用、版本 | 管理员授予的永久额度规则 |
| `quota_ledger` | `entry_id`、`user_id`、resource、amount、kind、request_id、created_at | 追加式 grant/reserve/commit/release/adjust 账本 |
| `quota_reservations` | `id`、`user_id`、request_id 唯一、amount、状态、过期时间 | 并发安全预占 |
| `requests` | `id`、用户、Key、协议、endpoint、model、request_hash、状态、结果摘要 | 全部业务请求的统一记录 |
| `idempotency_keys` | scope、client key、request_hash、request_id、响应/任务引用、状态 | 幂等和冲突检测 |
| `upstream_accounts` | provider、region、credentials_ref、启用状态、能力、并发上限 | 不含秘密的账号目录 |
| `upstream_observations` | account、resource、value、source、observed_at、stale_at、原始响应摘要 | 积分/能力观测，不是假定余额 |
| `upstream_leases` | account、request/task、resource、状态、lease 过期时间、预估观测版本 | 账号和并发槽预占 |
| `jobs` | 用户、请求、类型、状态、输入/输出引用、恢复信息 | 视频等异步任务 |
| `job_attempts` | job、attempt、account、lease、上游 request id、状态、错误分类 | 重试和对账依据 |
| `assets` | 用户、内容摘要、大小、类型、存储引用、过期时间 | 用户级素材所有权 |
| `audit_events` | actor、action、target、request id、结果、脱敏 metadata | 不可抵赖的管理/安全审计 |
| `outbox_events` | 事件类型、payload、重试次数、状态 | Agent/指标/对账的可靠投递 |

`quota_ledger` 只能追加，余额是按事务计算或由可重建快照加速；任何调整都必须带管理员 actor、原因和关联事件。SQLite 使用 WAL、外键、检查约束和事务，不能通过普通 JSON 写入绕过。

### 5.2 额度语义

额度单位由管理员配置为逻辑 `resource_kind`，例如 `chat_request`、`text_token`、`image_job`、`video_job` 或其他明确资源；它们不是人民币，也不等于上游积分。没有配置成本策略的能力不得在额度模式下执行。

用户可用额度为：已授予 grant − 已结算 commit − 当前有效 reserve + 明确 release/adjust。任何事务都必须保证可用额度不低于零。上游观测和用户账本永远分开，不能把 `remaining_credits.json` 直接导入为用户 grant。

## 6. 预算、调度与结算规则

### 6.1 保守预算

每个 endpoint/model/resource 必须有版本化 `CostPolicy`：请求校验先计算最坏情况预估值，再做用户额度预占。策略必须说明单位、上限来源、是否支持按实际用量结算和未知结果处理。

- Chat/Responses/Completions：优先按客户端上限与服务端配置上限计算；缺少上限或模型政策时使用管理员明确配置的安全上限，不能由代码猜测单价。
- 视频/图像：按任务级预估单位预占；真实上游返回的费用未知时不宣称“实际扣除多少积分”。
- 素材：以配置的字节上限/保留策略执行容量控制；存储额度与上游积分不混用。
- 预算无法计算时返回可诊断的 `budget_policy_missing`，不先调用上游后补记账。

### 6.2 账号选择

调度候选必须同时满足：账号启用、凭据可用、能力/区域匹配、未冷却、lease 未超限，并且积分观测来源满足该策略的新鲜度要求。排序可以使用现有 expire-first/credit-first/weighted 等策略，但最终选择必须记录原因、观测版本和安全余量。

- `remaining_credits.json` 迁移为 `source=json_cache` 的旧观测，只能用于诊断或经策略允许的保守选择。
- 云端积分刷新失败时保留旧观测但标记 stale；对需要积分确认的资源默认 fail closed，不因 stale 数据继续消耗未知账号。
- 账号并发槽和积分预估在同一 lease 操作中占用；不能只增加进程内 `inflight` 后再异步挑账号。
- 账号选择不泄露给普通用户；管理员审计可看到脱敏 account id、来源和失败分类。

### 6.3 结算与未知结果

1. Core 事务创建用户 reserve 和请求幂等记录。
2. Scheduler 成功后创建上游 lease；失败则原子释放用户 reserve。
3. Agent 返回明确成功：按已知实际用量 commit，多余 reserve release；实际用量未知则按保守预占 commit，并记录 `actual_unknown`。
4. Agent 返回明确的上游拒绝且确认未接受：release 用户 reserve，记录失败。
5. 超时、连接断开、进程崩溃或上游无 request status 时：状态为 `unknown`，lease 延长到对账截止时间；对账确认未接受才 release，无法确认时按策略保守结算并要求管理员处理。

用户账本与上游观测之间不做一对一金额换算。任何管理端调整都必须生成 `adjust` 账本事件，不能直接改余额字段。

## 7. 状态机与恢复

### 7.1 请求状态

`received → validating → reserved → queued → dispatched → streaming/completing → succeeded | failed | cancel_requested | canceled | unknown → settled`

每次状态变更写入事件并带版本号；非法回退、重复结算和重复释放由数据库唯一约束/状态条件更新拒绝。同步响应已经物化时可以幂等重放；无法安全重放时返回原 request id 和当前状态，而不是再次调用上游。

### 7.2 异步任务

`created → queued → running → cancel_requested → canceled | succeeded | failed | unknown`

任务必须保存用户、幂等键 hash、请求 hash、attempt、lease、上游 request id、产物引用和最后心跳。Core 启动时：

- `queued` 任务重新排队；
- lease 未过期的 `running` 任务先查询上游状态；
- 无法查询的 `running` 任务转 `unknown`，不能直接标记 completed；
- Agent 恢复后可重新认领仅限于适配器声明可安全恢复的任务；否则等待对账或管理员决定。

### 7.3 取消和流式

客户端断开只表示“请求方不再接收”，不能假设上游已取消。Core 先记录 `cancel_requested`，Agent 尝试调用真实取消接口；取消不可确认时保留 lease/unknown，并按对账规则结算。流式响应的 request lease 直到流结束、取消确认或进入 unknown 才释放。

## 8. 幂等、错误和接口契约

### 8.1 幂等

幂等键作用域为 `user_id + endpoint + client_idempotency_key`，数据库保存规范化请求 hash。相同键不同请求体返回 409 `idempotency_conflict`；相同请求返回原 request/task 状态或可安全重放的结果。视频、图像、素材上传和任何可能重试的非流式调用要求客户端提供 `Idempotency-Key`；服务器生成的 request id 不能替代客户端幂等键。

### 8.2 兼容响应

| 协议 | 成功与流式要求 | 统一错误映射 |
|---|---|---|
| OpenAI Chat/Completions | 保留标准 JSON、SSE、`[DONE]`、usage 结构 | `invalid_request_error`、`authentication_error`、`rate_limit_error`、`insufficient_quota`、`upstream_error` |
| Responses | 保留 response/event 类型和 request id；不伪造未实现事件 | 映射到 Responses 错误对象，附内部可追踪 request id |
| Anthropic Messages | 保留消息结构、thinking/内容块和 SSE 终止事件 | 使用 Anthropic `type`/`error` 结构，不混入 OpenAI JSON |
| 视频/素材扩展 | 明确任务/资产 schema、轮询状态、content 授权 | 统一 HTTP 状态，错误体携带稳定 `code` |

所有错误都不能透露账号凭据或其他用户资源。`/v1/models` 只列出当前实际可路由、且模型能力已被目录或适配器确认的条目；未知能力显示为不支持而不是猜测支持。

## 9. 迁移方案

### 9.1 迁移前保护

迁移前停止写入旧 API 服务，备份整个数据目录并记录文件 hash、Schema 版本和迁移日志。旧 JSON 在验证完成前只读保留，绝不原地删除或覆盖。

### 9.2 JSON 到 SQLite 映射

| 旧数据 | 导入方式 |
|---|---|
| `api_keys.json` | 明文 Key 不直接进入数据库；按管理员映射生成待轮换 Key，原 key 立即标记为迁移遗留/禁用 |
| `api_usage.json` | 导入为历史 usage 事件/聚合，只用于报表，不转换成用户额度扣费 |
| `api_pool.json` | 导入账号目录、策略和能力配置；JWT/凭据写入受保护凭据存储，数据库只留 reference |
| `remaining_credits.json` | 作为带 `source=json_cache` 的历史 observation，不生成 grant，不宣称实时余额 |
| `video_tasks.json` | 导入 jobs、attempts 和 owner 映射；`processing` 任务先置为 `unknown/reconcile_required` |
| `assets.json` 与文件 | 通过 Key→用户映射迁移 owner；无映射资源进入隔离区，不能被其他用户读取 |
| `api_gateway_settings.json` | 导入 Core 配置快照；端口、bind、CORS 等安全字段经过 allowlist 校验 |

迁移要求：每个记录可追溯到源文件和源 hash；计数、owner、状态和资源数量执行 parity report；缺失 Key→用户映射、损坏 JSON、重复 id 或未知状态必须中止该批导入并报告，不静默猜测。

### 9.3 切换与回滚

先以 Mock 上游运行 shadow/parity 检查，再按 `models → chat → assets → video → admin` 逐类切换。每一类切换后只有 SQLite 写入，旧 JSON 不再双写，避免双账本。出现问题时关闭新路由 feature flag、保留 SQLite 现场和旧 JSON 备份；回滚不把新账本反写成旧 JSON，恢复必须由审计过的迁移工具完成。

## 10. 分阶段交付

### Phase 0：设计与测试基线

完成本设计、Schema 迁移工具、Mock upstream、协议 contract tests、JSON parity report 和安全日志审查；不触碰真实生成。

### Phase 1：安全最小闭环

实现 Core SQLite、用户/Key/scope、非流式 Chat 的统一 request、用户额度 grant/reserve/commit/release、幂等和审计；接入 Mock upstream；保留现有协议适配器但只切换经过测试的 Chat/Models 路径。

### Phase 2：多账号积分感知调度

实现 Agent lease、账号能力/积分 observation、stale/fail-closed、并发槽、冷却和错误分类；Trae/WB 适配器先在模拟响应下验证，然后用只读刷新验证真实观测，不直接执行真实生成。

### Phase 3：流式、素材和异步视频

接入 Chat/Responses/Anthropic 流式 lease 生命周期；素材迁移为用户所有权和范围校验；视频使用持久化 jobs/attempts、取消请求、启动恢复、重试和对账。

### Phase 4：Tauri 管理与部署

补齐用户/额度/账号/策略/审计管理界面、Core/Agent 进程编排、Nginx/FRP 单入口部署、安全 bind 默认值、备份恢复和运维手册；每个公网域名仍通过同一授权模型。

## 11. 测试和可观测性

### 11.1 必须通过的测试

- 额度账本：并发 reserve 不超额、重复 commit/release 无副作用、unknown 不被错误释放、管理员 adjust 可审计。
- 身份隔离：不同用户不能读取任务、素材、usage、幂等记录或管理资源；请求体伪造 `user_id` 无效。
- 幂等：相同键重放、不同 hash 冲突、服务重启后仍可识别；视频提交只产生一个 job。
- 调度：积分新鲜度、过期账号、区域/能力不匹配、冷却、并发 lease、失败转移和所有 lease 回收。
- 恢复：Core/Agent 在 queued/running/streaming/unknown 各状态重启，结果符合状态机；不会把未确认任务标成成功或自动重复扣费。
- 协议 contract：Chat、Responses、Anthropic、Completions 的非流式/流式成功和错误结构；embeddings 保持明确 501。
- 迁移：JSON 损坏、owner 缺失、重复 key、旧 processing task、旧缓存余额均产生可审计报告。
- Mock upstream：成功、拒绝、慢响应、断流、重复响应、取消不可用和状态查询不可用；不调用真实消耗型接口。

### 11.2 日志与指标

所有请求携带稳定 `request_id`；结构化事件至少包括用户脱敏标识、协议、模型、状态、预算/预占 id、account 脱敏 id、attempt、耗时、错误分类和上游 request id。指标包括认证失败、reserve 成功/拒绝、active lease、stale observation、unknown task、恢复数量、按协议的成功率/延迟和迁移 parity 差异。日志默认不包含 prompt、完整输出、Key、JWT、Cookie、资源 token 或完整上游响应。

## 12. 设计验收标准

设计进入实施计划前必须满足：

1. 所有业务请求都有可追踪的用户、请求、预算和状态记录。
2. 用户额度和上游积分/余额有不同表、不同语义和不同结算路径。
3. 并发请求在同一事务/lease 规则下预占，不依赖进程内计数保证不超额。
4. API Key、用户资源、素材、视频和任务状态均有明确隔离边界。
5. 重启、断流、取消、超时和未知上游结果都有可执行的恢复/对账路径。
6. 未验证的上游能力、计费和余额不会被代码或文档假定为事实。
7. 现有 JSON 和脏工作树均可保留、回滚且不被隐式覆盖。

本文件是设计基线；用户审阅并确认后，下一步只生成实施计划，不直接跳过计划开始编码。

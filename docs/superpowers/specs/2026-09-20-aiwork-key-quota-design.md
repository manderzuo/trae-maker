# AI Work Assistant 用户总上限与 API Key 永久额度设计

日期：2026-09-20
状态：已获方案 1 设计确认，待书面规范审阅
适用仓库：`E:\\AIWORK\\workspace\\TraeWorkAssistant`

## 1. 背景与已核验事实

当前 Core 已具备用户、API Key、请求幂等、用户级额度账本、用户级预占、账号 lease 和管理员管理界面。当前数据库 schema 为 v11：

- `requests` 已保存 `user_id` 与 `api_key_id`，所以请求可以稳定关联到用户和具体 Key；
- `quota_ledger` 与 `quota_reservations` 目前只按 `user_id` 计算，不能表达 Key 之间的独立永久额度；
- `core_quota_grant` 和 `CoreAdminPanel` 目前只支持向用户发放额度，不能向单个 Key 发放额度；
- `remaining_credits.json`、Trae/WB 账号观测和用户额度账本是不同语义，不能互相 1:1 转换；
- 现有视频、素材、流式和调度路径已经使用 Core 用户身份与请求状态，Key 额度应接入同一个预占/结算事务，不能在路由外再维护一套余额。

因此，本阶段不是给公开查询增加字段，而是补齐“Key 独立预算 + 用户聚合上限”的权威约束层。

## 2. 目标与非目标

### 2.1 目标

1. 每个启用的 Core API Key 可以对每个 `resource_kind` 配置独立的永久额度；同一个 Key 在多台设备上共享同一额度。
2. 同一用户的多个 Key 不能绕过可选的用户级总上限；用户总上限和 Key 额度在同一 Core 事务中检查。
3. 一次请求只有一个稳定的 request/reservation/settlement 关联；用户约束和 Key 约束的账本事件使用同一个事件组，不产生两次实际扣费。
4. Key、用户和上游执行账号继续保持三种不同身份；Key 只作为授权入口，不绑定某一个上游账号。
5. v11 已有用户额度和未结束预占不被静默复制到所有 Key；旧额度必须由管理员显式分配或继续停留在受限迁移状态。
6. 普通用户只能查询当前 Principal 可见的有效额度和脱敏流水；管理员可以按用户、Key 和资源类型审计预算状态。
7. Mock 上游和离线 Core 测试可以证明并发安全、幂等、结算、恢复和隔离，不宣称任何真实上游计费规则。

### 2.2 非目标

- 不推断 Trae、WorkBuddy 或其他上游的真实单价、扣费顺序或可退款规则；
- 不把上游余额、`remaining_credits.json` 或账号观测导入为用户/Key 额度；
- 不在公网部署第二份用户账本；
- 不通过 Key 额度改动旧版 legacy API Key 的每日请求限额；两套鉴权/限额在迁移期继续分开；
- 不因为 Key 预算不足自动换 Key、换账号、降低模型、降低视频参数或增加重试预算。

## 3. 核心语义

### 3.1 两层约束、一个实际消费

Core 使用两个独立的预算约束账户：

- **Key budget**：某个 `api_key_id + resource_kind` 的永久额度，是请求能否执行的第一道硬约束；
- **User cap**：某个 `user_id + resource_kind` 的可选用户总上限，限制该用户所有 Key 的聚合消耗。

用户实际可执行额度为：

```text
effective_available = min(key_available, user_cap_available)
```

如果用户没有配置 User cap，则只应用 Key budget；如果用户已配置 User cap，则任何 Key 都必须同时满足两层约束。`held` 表示当前 Key 的有效预占，`settled` 表示当前 Key 已明确结算的消费；同一请求在 User cap 层的约束事件不再向用户视图重复累加。

`resource_kind` 必须保持显式且不可隐式互换；例如 work/general 等不同额度类别分别记账，不能因为某一类不足就折算、挪用或自动改用另一类。

物理实现使用同一套 Core 预算存储和同一个 `event_group_id` 关联一笔请求的多层约束事件。User cap 和 Key budget 可以各有一条约束记录，但它们不是两次扣费；实际用户消费只按 Key budget 事件统计一次。

### 3.2 预占和结算不变量

对一个新的 Core enforce 请求：

1. 从认证得到的 Principal 读取 `user_id`、`key_id` 和 scopes；请求体、查询参数和普通转发头不能覆盖它们。
2. 由已启用的 `CostPolicy` 计算用户预占金额和资源类型；无法计算则在任何余额变更前拒绝。
3. 在同一个 SQLite Immediate transaction 中校验 Key budget、User cap（若存在）、用户状态、Key 状态、幂等键和请求哈希。
4. 事务内创建唯一 request/reservation，并记录 Key 预算约束、可选 User cap 约束和 `event_group_id`；任何一层不足都整体返回 `insufficient_quota`，不能只扣一层。
5. 明确成功时只将同一事件组结算一次：Key budget 记录 commit，User cap 记录对应约束释放/结算；实际使用未知时保留保守占用并转为 `unknown`。
6. 明确失败且确认未接受时释放两层预占；超时、断开、进程崩溃和上游状态未知时，两层都保留到 recovery/reconcile 决策。
7. 重复 settle、重复 release、重复 reconcile 和同一幂等键重试都必须是幂等的；同一 Key 更换设备不能绕过数据库约束。

上游账号 lease 仍是第三个独立资源约束。用户/Key 额度预占成功不代表上游已经接受请求，上游 lease 失败时必须释放用户侧预占；上游结果未知时不得自动退款或换号重放。

## 4. 数据模型与 schema v12

### 4.1 预算账户

新增明确的预算账户目录（名称可在实施计划中按现有命名约定落地为 `quota_budget_accounts`）：

| 字段 | 约束 | 作用 |
|---|---|---|
| `id` | 稳定内部 ID | 关联账本和预占 |
| `scope` | `user_cap` 或 `key` | 区分用户上限与 Key 额度 |
| `user_id` | 外键 `users` | 所有预算的归属用户 |
| `api_key_id` | `scope=key` 时必填，外键 `api_keys` | Key 预算的唯一所有者 |
| `resource_kind` | 非空 | 文字、视频或其他逻辑额度单位 |
| `enabled` | 0/1 | 管理员可暂停预算账户，不删除流水 |
| `version` | 正整数 | 结算时记录使用的预算配置版本 |
| 时间字段 | UTC 毫秒 | 审计和迁移 |

约束要求：同一 scope、用户、Key、资源只能有一个有效账户；Key 必须属于同一用户；禁用或撤销的 Key 不得创建新的预算预占。

### 4.2 预算流水与事件组

现有 `quota_ledger` 在 v12 迁移为可区分账户 scope 的追加账本，至少增加：

- `budget_account_id`：对应预算账户；
- `event_group_id`：同一请求的 User cap 与 Key 约束事件使用同一组；
- `api_key_id`：Key scope 事件必须有值，User cap 事件仅作为审计关联；
- `budget_version`：预占时锁定的版本。

现有 `event_kind` 继续使用 `adjust`、`reserve`、`commit`、`release` 等明确类型。账本仍只追加，不覆盖历史记录。`actor_user_id`、`reason` 和完整审计元数据只供管理员视图，不进入普通用户投影。

### 4.3 预占记录

`quota_reservations` 保持一个请求一个逻辑 reservation，并增加：

- `api_key_id`；
- `key_budget_account_id`；
- 可选 `user_cap_account_id`；
- `event_group_id`；
- 各账户预占是否已写入的内部一致性字段，或等价的事务约束。

已有 `requests.api_key_id` 与 reservation 的 Key 必须相等。迁移不能将一个 reservation 重新绑定到其他 Key。

### 4.4 旧数据迁移

v11→v12 在单一 SQLite transaction 中完成：

1. 为已有 `user_id + resource_kind` 的用户额度建立 `user_cap` 账户；已有用户流水原样归入该账户并保留历史 `entry_id`、时间、原因和审计关系。
2. 不把已有用户余额复制给该用户的每一个 Key；没有明确 Key 分配的额度标记为 `legacy_unassigned`。
3. 管理员通过显式迁移操作把指定数量从 `legacy_unassigned` 转移到指定 Key budget：用户账户追加负向 adjust，Key 账户追加正向 adjust，使用同一个 migration/event group，并要求原因和管理员身份。
4. 正在 held/unknown 的旧 reservation 不自动换 Key、不自动退款；迁移报告列出其 request、原 Key 和状态。Core enforce 在相关映射未完成前 fail-closed。
5. 迁移失败必须整体回滚，不生成部分预算账户或部分流水；旧 JSON 只读保留，不能作为新 Core 的第二写入源。

`core_mode=off/shadow` 继续按现有兼容路径运行；切换到 enforce 前，管理员必须完成未分配旧额度和未结束预占的迁移检查。

## 5. Core 接口与错误边界

### 5.1 内部 Core API

在现有用户级接口之外增加：

- `key_quota_grant_as_admin`：向指定用户的指定 Key 发放/调整永久额度；必须校验管理员 Principal、Key 归属、资源类型、正整数金额和非空原因；
- `key_quota_balance_as_admin`：查看某个 Key 的可用、held、settled、预算版本及迁移状态；
- `key_quota_allocate_legacy_as_admin`：把明确指定的 legacy 用户额度转移到指定 Key，不允许批量复制到多个 Key；
- 用户级 `quota_balance_as_admin`：继续用于 User cap 和迁移前用户余额，返回 scope 明细，避免把 Key 消费误当成两次用户消费。

原有 `core_quota_grant` 保留为用户总上限/迁移兼容操作；新 Key 额度必须经过明确的 Key 目标接口，不能通过缺省 `user_id` 猜测目标 Key。

### 5.2 公共查询

`GET /v1/usage?limit=<1..100>` 继续要求 Core enforce 和 `usage:read`；保留现有响应外层字段（包括 `object` 和 `limit`）以兼容客户端，但成功投影改为当前认证 Principal 的 Key 视角：

- `balances.available`：当前 Principal 的 effective available；
- `balances.held`：当前 Key 的有效 held，不把 User cap 约束重复相加；
- `balances.settled`：当前 Key 的明确 settled 消费；
- 可选的 `key_available`、`user_cap_available` 和 `key_quota_configured`：仅表示当前 Principal 可见的边界，不返回其他 Key；
- `ledger`：只返回当前 Key 的脱敏 Key-scope 事件，继续隐藏 actor、reason、凭据、prompt、上游账号和完整内部 ID。

Key 预算未配置时不返回用户级 legacy 余额冒充 Key 余额，而返回稳定的 `key_quota_not_configured` 状态；普通用户不能自行创建或修改预算。

### 5.3 错误和 fail-closed

- 未认证：401 `unauthorized`；
- 缺 scope：403 `insufficient_scope`；
- Core off/shadow：501 `core_usage_not_enabled` 或对应执行端点未启用错误；
- Key 未配置、预算版本失效或迁移未完成：409 `key_quota_not_configured`；
- 两层任一预算不足：429 `insufficient_quota`，不透露其他用户或账号余额；
- 存储/迁移一致性失败：通用 500，不返回底层 SQL 或账本敏感字段。

## 6. 管理界面与部署边界

Core 管理界面增加 Key 级额度区域：用户筛选 → Key 筛选 → resource_kind → 发放/迁移/查看余额。页面只显示 Key prefix、scope、预算状态、available、held、settled 和迁移提示，不显示明文 Key、digest、凭据或上游账号。

管理员 API Key 继续只保存在管理会话内存中；普通用户 Key 不获得管理命令权限。公网、LAN 和本机都进入同一 Core 预算事务，不信任 Host、域名或客户端提供的用户标识。

Nginx、FRP 和三个域名只承担传输/反向代理。任何域名能力隔离必须由 Core 路由和 scope 实现；不能通过域名名称推断 Key 额度或管理员权限。

## 7. 审计、观测和恢复

每次预算调整、迁移、预占、结算、释放和未知结果都记录：脱敏 actor、用户/Key 标识、资源、金额、事件组、预算版本、request/reservation ID、结果和时间。日志不得包含明文 Key、JWT、Cookie、prompt 或完整上游响应。

指标至少包含：Key 预算不足、User cap 不足、未配置 Key、迁移待处理数量、双层预占成功/回滚、unknown hold、重复结算、恢复和 reconcile 结果。账号积分 observation 继续作为上游调度输入，不进入用户 Key 账本。

恢复时按事件组检查两层约束是否同时存在：缺少任一侧事件、状态不一致或上游结果未知均进入 `reconcile_required`，不得自动补扣、自动退款或换 Key 重放。

## 8. 验证标准

### Core 与迁移

- v11 数据库迁移到 v12 后 schema、外键、WAL 和旧用户历史流水保持可读；失败全量回滚；
- legacy 用户额度不会自动复制到多个 Key；显式分配前 enforce fail-closed；
- Key A 的额度不能被 Key B 消耗；同一 Key 多设备共享余额；撤销/禁用 Key 不能创建新 reservation；
- User cap 配置后，多个 Key 的并发预占总和不能超过 User cap；无 User cap 时只受 Key budget 约束；
- reserve、commit、release、unknown、recovery、reconcile 和重复通知在两层约束下保持幂等，不发生双扣或错误退款；
- 同一 idempotency key 同参只创建一个 request/event group，同键异参稳定冲突。

### API、管理与前端

- 公共 `/v1/usage` 只返回当前 Key 投影，不能读取其他 Key、用户或上游账户；
- 管理命令要求真实 admin Principal，不接受伪造 admin ID、用户 Key 或明文敏感字段回显；
- Core Admin Panel 可发放 Key 额度、迁移旧额度、查看双层余额和待迁移状态；
- Core 全量 Rust 测试、Tauri 聚焦/全量测试和前端 `npm test` 在 `D:\\gpt` 临时目录/target 下通过；测试使用 Mock，不调用真实生成或真实计费接口；
- 文档明确“上游余额不转换为用户额度”，并记录未核验的真实计费/余额/协议边界。

## 9. 实施顺序

1. 先为 schema v12、预算账户模型和双层预占写失败测试，确认旧数据迁移和回滚边界。
2. 实现 Core 预算账户、Key 管理方法、双层 reserve/settle/recovery 和审计投影；不接真实上游。
3. 扩展 `/v1/usage` 与管理员 Tauri 命令/类型/UI，保留旧用户级接口的兼容语义并增加显式迁移入口。
4. 更新统一网关设计、Phase 4E 运维说明和用户手册，写明 enforce 前置条件与 fail-closed 行为。
5. 运行 Core/Tauri/frontend 全量验证，检查用户已有脏文件未被暂存，再决定是否进入下一阶段。

## 10. 方案 1 实现状态与验证记录

Task 1–5 已完成实现并提交，Task 6 补充本节及运维文档后作为方案 1 的最小闭环记录：

- 数据库已从 v11 迁移到 schema v12。预算目录使用 `quota_budget_accounts`，迁移在同一 SQLite 事务中完成；失败整体回滚，不留下半套账户或流水。
- 旧用户额度只进入 `user_cap` 或明确标记为 `legacy_unassigned`，不会静默复制到该用户的每个 Key。管理员必须按指定数量、Key、资源类型和原因执行显式迁移。
- enforce 前必须完成 Key 预算准备、legacy 分配和未结束 reservation 核对；缺 Key 预算、版本不可用或迁移未完成时返回稳定的 `key_quota_not_configured`（409），不回退为 legacy 用户余额。
- 请求生命周期使用同一个 `event_group_id` 关联 User cap 与 Key budget 约束；Key、User cap、上游账号 observation/lease 三种余额保持分离。`unknown` 只进入 `reconcile_required`，不能自动退款、补扣或换 Key 重放。
- `/v1/usage`、Tauri 管理命令和 Core Admin Panel 均以当前 Principal 的 Key 视角工作；公网、LAN、本机共用同一 Core 认证和预算事务。Nginx/FRP 只负责传输，不能决定权限或额度。
+ 测试使用 Mock，不把真实上游余额、价格、计费协议或生成结果当作验收证据；临时日志和 Rust target 使用 `D:\gpt`，不在 C 盘进行读写测试。

本轮最终验证记录（2026-09-20）：

- 文档契约检查：5 份目标文档、9 个标记全部通过；输出位于 `D:\gpt`。
- Core：fresh `D:\gpt\aiwork-final-core-target`，18 个测试套件共 107 passed、0 failed、0 ignored。
- Tauri：fresh `D:\gpt\aiwork-final-tauri-target`，458 passed、0 failed、4 ignored。
- 前端：5 个测试文件、29 passed、0 failed；测试缓存使用 `D:\gpt\npm-cache`。
- `git diff --check` 通过；用户已有脏文件保持未暂存，未把真实上游生成、余额或计费接口作为测试依赖。

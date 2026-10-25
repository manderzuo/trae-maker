# AI Work 助手项目交接

最后核对：2026-10-24。本文以仓库源码为主、项目说明为辅，帮助新维护者找到系统边界、数据、运维要求和当前未决事项。它不是生产部署凭据清单；不在仓库中保存账号、API Key、JWT、Cookie、DPAPI 文件、私密备份或含真实用户内容的日志。

## 一页摘要

AI Work 助手是 Windows 桌面控制台和本机 API 网关。它管理 Trae Work、Trae CN、WorkBuddy/CodeBuddy、豆包等客户端的账号、签到、权益和登录态；网关兼容 OpenAI、Anthropic 和 Codex 客户端，并承载 Seedance 图片素材、视频任务和 Work 积分流程。项目逐步加入 `aiwork-core`，集中处理 Core 用户、Key、逻辑额度、上游资源观测、租约、素材、任务与费用账本。

关键边界：旧 API 账号池、Core 用户额度、桥接账本是三套不同的身份和状态系统。上游积分、Core grant 和已核实的实扣回执含义不同，不能互相替代。付费执行若结果不确定，必须保留原任务、预算和 hold，禁止自动重发、删记录或把未知当作零费用。

## 仓库基线与可信状态

| 项目 | 仓库核对结果 |
| --- | --- |
| 产品版本 | `package.json`、`src-tauri/Cargo.toml` 为 `3.4.5` |
| 交接基准 | 分支 `fix/aiwork-capacity-recovery`，HEAD `99eaeb3`，最后提交日期 2026-10-16；该分支当时与远端同名分支一致，工作区干净 |
| 主分支 | 此工作区的本地 `main` 与远端 `main` 明显分叉（当时本地 ahead 1 / behind 81）。合并前需先核对实际目标和共同基线 |
| 当前代码状态 | 最近修改集中于桥接预算容量、过期预算回收、工作进程退出事实及历史费用核对 |
| 部署状态 | 本次交接只检查了仓库，未核对正在运行的桌面 EXE、API 7864、桥接数据库、远端 Core 或反向代理。不要根据仓库 HEAD 推断生产已部署相同版本 |

版本字段、架构文档和历史部署记录存在滞后：`AGENT.md` 标题还写 v3.3.5，`tech-framework.md` 写 v3.2.7；当前桌面 package/crate 是 v3.4.5。更重要的是，Core 源码 `CURRENT_SCHEMA_VERSION` 为 **20**，桌面桥接账本 `BRIDGE_SCHEMA_VERSION` 为 **11**；多份旧运维文档仍写 Core v9 或 v12。准备迁移、回滚或复核部署前必须查看目标版本源码的迁移常量和代码，不能照抄旧文档中的 schema 数字。

## 产品范围

- **Trae Work / Trae CN**：发现安装与当前账号、OAuth/JWT 续期、账号分组、签到与积分查询、按应用保存和恢复登录快照、设备标识管理、登录态接力及独立实例控制。统一 Trae CN 是旧 Work 与 IDE 合并后的客户端；授权来源、所选安装和快照路径须成套处理。
- **WorkBuddy / CodeBuddy**：账号与 auth 文件发现、签到、成长中心、额度和官方用量查询、会话资料备份，以及 WorkBuddy 上游模型路由。两者复用一部分账号/协议，但分别有本地数据与调用场景。
- **豆包**：独立账号与会话凭证、MITM 捕获回写、按账号快照、续期/额度检查、会话数据库备份与导出。不得把豆包的 Web 私有签名或 Cookie 逻辑当成已获准的对话 API 连接器。
- **统一 API 服务**：本机 API Key、账号池、模型目录、限流、健康状态、协议适配和日志。聊天端点包括 OpenAI Chat Completions、Anthropic Messages、Codex Responses；另有图像、素材和 Seedance 视频能力。不同端点是否走 legacy 路径或 Core 取决于服务模式与就绪条件。
- **Core / 桥接账本**：Core 管理用户、用户 Key、scope、逻辑额度、幂等请求、上游观测与并发 lease、素材及视频 job/attempt；本机桥接数据库承担专用 Work 账号容量、Prepared/P/R/D、发送身份、回执、退出观察和恢复记录。
- **交付工具**：PowerShell 切号桥、Python 签到/代理/多应用辅助脚本、Seedance CLI/MCP skill、桌面安装包脚本。另有一个仍在开发中的独立星链路由器 Rust crate。

## 代码地图

```text
src/                         React/TypeScript 桌面界面
  App.tsx, store.ts, types.ts
  pages/                     Dashboard / Accounts / Checkin / Credits / Logs
                             ApiService / CoreConsole / Settings
  pages/accounts/            登录、发现、分组、快照、导入等账号组件
src-tauri/src/               Tauri/Rust 桌面后端、命令与 API 服务
  main.rs, state.rs, models.rs, jwt.rs, native_client.rs
  commands/                  OAuth、账号、签到、切换、快照、代理、网关、Core 等
  api_server/                legacy 网关、协议转换、Core 接口和桥接账本
src-core/src/                可复用 Core SQLite 业务库
src-core/tests/              Core 数据层与状态机集成用例
src-python/                  签到、代理、Trae/WorkBuddy/豆包辅助与 MCP 服务
src-ps/                      Windows 客户端切换与进程桥接
starlink-dimension-router/   实验性独立 Axum 用户/API/管理员入口
skills/aiwork-seedance/      Seedance 命令行与 MCP 客户端技能
scripts/                     开发、Python runtime、安装包、测试与部署辅助
deploy/                      反向代理和隧道样例
docs/                        使用、架构、部署、审计、恢复和迁移记录
```

React 通过 `src/lib/tauri.ts` 调用 Rust 命令并订阅事件，状态由 `src/store.ts` 归约；Rust 命令和 DTO 是桌面操作的权威实现。`tauri.conf.json` 把 Python、PowerShell 作为 bundle resources，并在 Tauri 构建前准备 Python runtime。PowerShell 负责依赖 Windows 客户端进程和文件布局的操作。修改命令名、参数字段或 JSON DTO 时，必须同时核对前端类型、Rust 注册和脚本调用。

## 主要请求与状态链路

### 桌面账号和登录

OAuth/BitBrowser 或用户手动提供的登录数据进入 Rust 账号流程，按选中应用解析 UID 和签发方，再加密保存凭据；当前 JWT 可续期时验证响应 UID 后更新。快照分应用存储，文件结构依客户端类型而不同。旧账号的 OAuth issuer 不可因目录迁移而改写；登录 Cookie、账号 JWT 与项目会话映射不能跨 UID 拼接。

注册与第三方验证码、短信、CAPTCHA、平台风控仍由用户亲自完成。项目只支持把合法登录结果安全接入账号池；不做自动绕过验证、批量注册或设备风控规避。Trae Work 和统一 Trae CN 共用账号池的一部分路径，但快照目录及客户端版本必须解析一致。`docs/trae-cn-migration.md` 说明 TRAE CN 特有规则。

### API 网关

外部客户端请求本机 axum 服务，先由相应 API Key 鉴权、请求大小/并发限制和模型路由处理。legacy `ApiPool` 路径兼容 Trae SOLO 与 WorkBuddy 多池、冷却、粘性、Key 白名单和日统计；聊天格式投影、模型目录、图像端点与 SSE 转换分在 `src-tauri/src/api_server/` 子模块。Seedance 使用 Work 资源选择、素材上传/校验、幂等提交、视频任务查询及受 Key 保护的内容返回。普通聊天使用的 General 积分不能冒充 Work 视频容量。

外部常见路径为 `/v1/models`、`/v1/chat/completions`、`/v1/messages`、`/v1/responses`、图像生成/编辑、`/v1/assets` 与 `/v1/videos/generations`。部分 Core/enforce 路由还需要 scope、严格幂等键、fresh 观测、策略、grant 和就绪的真实适配器；缺少前提时应安全失败，不能回退到旧池伪装成功。

### Seedance 与费用

旧桌面网关桥接账本和 Core 用户额度是上下游两层。大致顺序是：用户 Key/权限检查 → 幂等预检与额度/素材校验 → 选择专用 Work 账号及有效容量 → 持久准备和发送栅栏 → 上游请求 → 读取视频 SSE/任务状态 → 取得可信的实际积分回执 → 结算并提供产物。

只有明确未接受的拒绝或确认取消，才能走各自规则释放相应 hold。响应丢失、超时、断线、进程退出、读流损坏、缺账单均不能推定上游未执行。`unknown` 保留原 identity 并待核对；晚到的精确回执可以补入账。`bridge-billing.sqlite3` 的恢复接口不是退款、清账、重发或打开收费通道的快捷方式。

## 身份、额度和数据库

| 系统 | 存储/凭据 | 权责 |
| --- | --- | --- |
| 桌面与 legacy API | 数据根目录下 `data/checkin_accounts.json`、`api_keys.json`、`api_pool.json` 等；Windows 原生 token/DPAPI/Stronghold 路径按应用实现 | 登录账号、旧式外部 Key 日额度、模型池与账号调度；账号列表对敏感值掩码，按需命令才取完整值 |
| Core | `data/core.sqlite3`；源码当前 schema 20，SQLite WAL | Core 用户、角色、Key/scope、逻辑 quota ledger、request/reservation、上游账号/观测/lease、assets、video jobs 与 attempts。`credentials_ref` 必须是不透明引用，不保存上游密钥 |
| 桥接计费 | `data/bridge-billing.sqlite3`；源码当前 schema 11 | 本机实例/代际、Work 容量、Prepared/P/R/D、已发请求身份、费用回执、unknown 与恢复证据 |
| 任务和文件兼容层 | `data/video_tasks.json`、`data/assets/` 或配置的视频目录、`data/videos/` 等 | 单机/legacy 视频索引、参考素材和产物；与 Core 表并存时不能只备份一侧或假设自动导入 |
| 独立星链路由器 | 单独 `STARLINK_ROUTER_DATA_DIR`、`router.json`；默认端口 7865，默认 bind `127.0.0.1` | 计划承接用户认证、管理员界面、素材及 Core/AI Work 桥接；不得与桌面 7864 网关状态混为一谈 |

桌面数据根优先 `AIWORK_DATA_DIR`，兼容旧别名 `AIWORKDATA_DIR`；无显式覆盖时 Windows 默认 `%APPDATA%\AIWorkAssistant`，Unix 会使用 XDG/HOME fallback。通常含 `conf/`、`data/`、`logs/`；视频文件还可使用 `AIWORK_VIDEO_DIR`。显式数据目录代表便携/服务部署，不应顺带导入桌面 APPDATA 中的旧账户。

归档和恢复都要保留同一数据根内彼此关联的 SQLite、WAL/SHM、JSON 索引、凭据密文、素材及视频产物。写入活动期间不要只复制单个数据库文件。采用更高 schema 的二进制打开后，不得用旧二进制启动、恢复旧数据库覆盖新账务或手工删 unknown/hold；先停派发、确认进程/租约静止、按运维文档做可校验备份。

## 安全及修改约定

- 永不提交运行数据根、账号/Key/DPAPI 凭据、实际视频内容、私密备份、发布包或原始上游响应；把 debug API 请求日志当作敏感资料。
- 用户 Key、Core Key 和内部桥接管理员 Key 权限不同，不能在客户端或功能间互换。Core 管理员 Key 应最小范围保存；签发值只展示一次。
- `off`、`shadow`、`enforce` 决定 Core 是否为请求权威来源。只有实现、绑定、配置和观测都符合启用门槛才可进入 enforce；否则使用明确定义的模式或 fail closed，不臆造容量与账单。
- 服务器运行时目录应与仓库隔离，API 限定可信监听地址/HTTPS 反代；7864 与 7865 的访问策略按部署拓扑单独限制。MITM 根证书仅在明确需要的本机代理场景安装。
- Tauri 命令参数和 DTO 是跨层契约；保持兼容或同步更新调用方。UI 使用自定义 Modal，不使用 `window.confirm()`。
- 品牌迁移采用复制保留旧目录，快照按 UID/应用隔离。身份不符、文件损坏或客户端版本差异时失败并要求恢复/重新授权，不能覆盖另一个账号的数据。
- 新增或修复能力时同步维护 `AGENT.md`、CHANGELOG 及相关流程文档；对照源码常量检查 schema 和运行状态后再更新历史说明。

## 开发、构建和验证

主要工具链：Windows 11、Node/npm、Rust stable/MSVC、Visual Studio C++ Build Tools、WebView2；Python runtime 由 `scripts/prepare_python_runtime.py` 在 Tauri 构建前准备。`src-python/requirements.txt` 目前包含 Python 辅助运行依赖。根 npm scripts：

```powershell
npm ci
npm run tauri dev
npm run build
npm run test
npm run tauri build
python scripts/rename_release.py
```

`npm run tauri dev` 是仓库约定的桌面开发入口；它为 Vite/Tauri 选取一致的可用端口。单独 Vite 预览不能验证 Tauri 命令或 OS 行为。Tauri 打包会运行 Python 资源准备及前端构建，安装包为 MSI/NSIS；发布版本按项目版本流程和用户授权执行，不因普通修复自动升版本。

Core 与桌面后端是独立 Cargo manifest：`src-core/Cargo.toml`、`src-tauri/Cargo.toml`；独立 router crate 在 `starlink-dimension-router/Cargo.toml`。项目操作规范要求 Cargo 命令离线锁依赖、测试使用隔离数据、清理并确认 `AIWORK_*` 环境变量，指定 `D:\gpt` 下的 target/log/TEMP/TMP。不要随手运行真实上游、真实收费操作或全量 Tauri 测试；详细命令和禁区见 `AGENT.md` 及 Core 运维文档。本文编写期间没有运行测试或构建。

`starlink-dimension-router` **目前未达到可直接当作生产服务器运行的状态**：crate 已有 RouterConfig、用户路由、管理员认证/路由、Core bridge、assets、视频计费与对账模块，但 `src/main.rs` 仍打印 `starlink-dimension-router skeleton` 后退出。部署文档描述的是目标接口和发布形态；动手部署前先检查入口、启动流程、配置、身份桥接和部署验证是否已实现。

## 当前工作与后续核对

### 最近源码主题

- TRAE CN 3.4 迁移：统一客户端身份贯穿安装解析、OAuth、设备签名刷新、切号快照、代理、签到与项目会话接力。交接细节在 [TRAE CN 迁移说明](trae-cn-migration.md)；[验证记录](trae-cn-migration-validation-2026-10-12.md) 有多个历史阶段，阅读标题所示时间和后续状态，不要把早期 SHA 当成当前版本。
- Seedance 生命周期：CLI/MCP 每次新任务为参考文件重新检查、上传并保存来源/幂等回执；读取旧上传 ID 前验证文件与凭据作用域。见 [参考素材生命周期修复](seedance-reference-lifecycle-repair-20261015.md)。
- 视频 SSE 与任务：1800 秒整体读取期限；同一响应只允许一次限定类型的临时 IO 错误恢复，不重放已发送 POST；unknown、账单冲突和迟到结果保持可核对。见 [视频读流修复](video-stream-read-repair-20261015.md) 与 [工作进程退出账务事实](worker-exit-accounting-20261015.md)。
- 桥接费用恢复：当前源码 bridge schema 11，新增释放账本和历史未知 worker 独立核账；自动释放要求具体失败证据和完整观测，未知执行仍有额外等待/精确账单条件。见 [桥接预算恢复](bridge-budget-recovery.md) 和 [费用核对记录](held-credit-reconciliation-20261014.md)。
- Core 状态机：当前源码 schema 20，已包含身份、授权、quota、上游观测/租约、assets、视频 job/attempt、管理投影和账务能力。旧文档的阶段号/SQL schema 有滞后，不能把早期“phase 验证已通过”误读为真实生产 connector 或生产部署就绪。

### 接手后先做的核对

1. 记录真实仓库路径、分支、HEAD、工作区状态、对应远端及目标发布分支；在解决本地 `main` 分叉前，不将修复分支直接合并过去。
2. 单独只读核对运行中的 AI Work/Core/Studio 进程、路径/hash、配置数据根、监听地址、桥接实例与代际、Core/bridge schema、在途任务和未知占用。不要输出或复制明文凭据。
3. 在 [唯一待办清单](product-optimization-backlog.md) 核实功能是否仍未完成；它和 10 月 12 日规格/计划之外的历史意向可能比当前源码更旧。
4. 定具体工作时先读 [项目开发约定](../AGENT.md)，再读相应专题文档。涉及数据/计费先写清楚失败和回滚边界；本地 Mock 成功不是上游账号、实扣或生产适配器验证。
5. 变更完成后核对跨层契约、版本/schema、安装资源、测试证据、备份和运行版本，并同步交接/操作文档。

## 项目文档地图

### 使用与产品

- [README](../README.md)：快速介绍、功能概览、开发入口。
- [用户手册](user-manual.md)：账号、签到、积分、API、代理与设置操作。
- [产品设计](product-design.md)：页面和初版功能的历史设计基线。
- [产品优化待办](product-optimization-backlog.md)：仓库声明的唯一功能待办；具体完成状态接手时要再对照源码。
- [技术框架](tech-framework.md)：早期分层、数据 DTO 和应用协议说明，部分细节已过时。

### 网关、Core 和容量账务

- [部署准备](server-deployment.md)：桌面服务、独立路由器、HTTP 入口、数据布局与部署目标；此文有 schema/实现状态陈旧内容。
- [Core 基础运维](core-foundation-operations.md)、[信用调度运维](credit-aware-scheduler-operations.md)：Core 模式、身份、quota、读者/调度器、备份与 unknown 规则；不要复用其旧 schema 版本数字。
- [专用 Work 账号与容量核对](bridge-dedicated-accounts.md)、[Reference 素材预算](bridge-reference-budgets.md)：账号容量授权、素材时长与风险预算。
- [桥接异常恢复](bridge-budget-recovery.md)、[未知 worker 退出记账](worker-exit-accounting-20261015.md)、[hold reconciliation](held-credit-reconciliation-20261014.md)、[QA61 capacity recovery](qa61-capacity-recovery.md)：schema、持久事实和费用核对阶段记录。
- [多路由器 LAN/FRP/Nginx 设计](lan-frp-nginx-design.md)：安全暴露本机网关的目标拓扑；部署前确认环境值和实际网段。

### Trae、Seedance 和视频生命周期

- [TRAE CN 适配](trae-cn-migration.md) 与 [迁移验证/历史版本](trae-cn-migration-validation-2026-10-12.md)。
- [视频迁移验收](video-migration-repair-20261012.md)、[运行流恢复历史](video-stream-recovery-20261012.md)、[当前读流边界](video-stream-read-repair-20261015.md)。
- [参考素材预检](reference-resource-readiness-20261013.md)、[失败请求稳定性](production-stability-20261013.md)、[Seedance 素材生命周期](seedance-reference-lifecycle-repair-20261015.md)。
- [Seedance skill](../skills/aiwork-seedance/SKILL.md)、其 [配置说明](../skills/aiwork-seedance/references/setup.md) 和 [API 说明](../skills/aiwork-seedance/references/api.md)；[MCP 接入](mcp-seedance.md) 描述客户端设置。
- [续写能力记录](seedance-continuation-capabilities.md)、[临时视频 fallback 历史](seedance-fallback-policy.md)、[本地尾帧提取](video-frame-extractor.md)：适用范围与有效期必须逐份阅读，历史风险阈值不等于当前授权。

### 其他专题和历史审计

- [WorkBuddy/豆包与协议](tech-framework.md)、[模型路由历史](model-routing-20260928.md)。
- [早期资源池/会话/远程网关计划](resource-pool-conversation-remote-development-plan.md)、[仓库同步历史](repository-sync-20261012.md)。
- [TRAE 文本和视频审计](trae-cn-text-and-video-audit-20261012.md)、[变更日志](../CHANGELOG.md)。

专题审计通常只描述当时的源码、线上或本地运行状态；比较版本时优先选日期最新且明确核实过部署 hash 的记录。产品规划、历史价格、签名链接有效期、账号余额和生产授权可能过期，重新启用前须作新检查。

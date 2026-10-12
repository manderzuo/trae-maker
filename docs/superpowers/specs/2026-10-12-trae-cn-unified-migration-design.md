# TRAE CN 统一客户端全链路迁移设计

日期：2026-10-12  
实施仓库：`E:/trae-studio/work/aiwork-capacity-qa61`  
基线：`c2520c05ec16a429de0d8d299fca11cd980d8d71`  
调研记录：`C:/Users/StarLink/Documents/ChatGPT/traework/.codex/research/trae-cn-migration-20261012.json`

## 目标与验收口径

用户要求把此前 TRAE WORK CN 的全部已有链路迁到统一 TRAE CN，包括注册入口、JWT 获取与续期、每日签到、账号切换；本设计同时覆盖账号发现、快照、项目接力、多实例、代理捕获、模型和媒体适配、定时签到与安装后的资源加载。

完成口径是：已升级用户的日常操作指向 Trae CN；旧账号与快照保持可读、可恢复；凭据获取和续期识别实际签发客户端；已验证兼容的上游能力保持可用；新增改动有隔离验证和可回滚交付。真实账号注册、交互式授权与计费生成分别报告实际验证程度。

## 已有证据

- 本机统一客户端产品版本 3.4.2，VS Code 基础版本 1.107.1。
- 官方迁移记录 status=completed，目标为 APPDATA/Trae CN；旧用户目录保留，旧 EXE 已移除。
- 2026-10-12 12:36:17 的 switcher.log 报“未找到 Trae Work 可执行文件”。
- 旧、新用户目录当前保存的 Cloud-IDE 登录账号不同，旧目录不能继续充当当前登录来源。
- 旧 data/profiles 有 17 个条目，data/profiles_trae 不存在。
- 两客户端的 tc 登录容器均能由现有解密算法通过完整性校验。
- 现有网关 /health、认证后的 /status、/v1/models 可用。
- 新版原生客户端的模型配置、资源地址查询返回 HTTP 200。
- 当前原生凭据、裸 JWT 的 x-ide-token、旧 Work 版本参数的模型配置查询返回 HTTP 200；solo_work_lite 仍存在。
- 新版 product.json 的 TRAE stable OAuth client_id 为 ono9krqynydwx5，SOLO stable 仍为 en1oxy7wnw8j9n。新授权与存量 refresh token 的客户端归属须分开处理，不能全局替换。
- 安装包仍包含 llm_utils_chat、tool_text_to_video_stream、素材上传、Seedance 等端点。
- 新客户端已使用 solo_hub 调度接口；是否影响会话接力须独立验证。

## 选择的迁移方式

集中解析“实际原生客户端”，沿用现有账号池和对外 API。兼容旧标识，保留旧数据，逐模块替换读取、启动和授权的客户端依赖。

不采用逐文件改名：TraeWork 和 solo_work_lite 中有仍有效的协议值，直接替换会破坏契约。
不重写账号池、积分账本或网关：这些模块已存在并有独立的安全与计费约束，本次只修改与客户端迁移相关的适配边界。

## 1. 原生客户端解析

增加 Rust 原生客户端适配模块，作为安装路径、产品版本、用户目录、逻辑目标、OAuth 配置、快照根目录及请求身份参数的统一来源。

选择原则：

1. 使用可验证的安装产品信息和迁移记录。升级完成或安装统一版 TRAE CN 的用户优先使用 Trae CN。
2. 用户显式选择的有效客户端配置予以保留；过期路径不能压过存在的新安装。
3. 旧 TraeWork 标识是兼容入口，由解析器映射到实际客户端；老客户端仍存在且未升级时继续支持。
4. Trae CN 旧 IDE 与统一版本的判断依赖产品信息，不仅依赖进程名称。
5. 除非调用明确指定，账号发现和自动切换以实际运行客户端的当前 tc 登录容器为准，不合并已保留旧目录为“第二个当前账号”。

现有 Tauri 命令名、snake_case DTO 和外部 API 路径保持兼容。新增字段使用 serde default，旧配置无需手动重写。

## 2. 注册与授权入口

注册/登录通过现有官方授权页及浏览器入口，不新增批量注册服务。

- 普通浏览器、BitBrowser、网页 token 导入使用统一客户端信息。
- 新建原生 PKCE 授权采用实际 TRAE 产品配置，ClientVersion 来源于安装包而不是旧 Work 常量。
- NativeOAuthRequest 保存本次 client_id，AuthCode 交换使用同一个 client_id；state、PKCE、loginTraceID 与回环来源校验保持严格。
- JWT 与 refresh token 的客户端归属写入账号元数据；导入、导出、vault 存取保持该字段。
- 存量凭据不能因升级被标记为新客户端签发；未知归属保留旧兼容路径，并在需要新授权时给出明确可操作结果。
- 网页来源 token 与原生 token 的现有区分保持，避免把短期网页 token 当作长效原生凭据。

## 3. JWT 获取、捕获和刷新

- 当前原生 JWT 从选中的实际用户目录读取；tc 解密只在本机内存中执行。
- Cookie/Local Storage 的兜底发现支持新 Trae CN 目录，旧目录作为历史来源而不是默认真源。
- 代理响应捕获携带可确认的 OAuth client_id 元数据；刷新必须使用凭据归属的 client_id。
- 刷新结果的 JWT 用户 ID 必须与目标账号一致；失败不得覆盖旧凭据。
- x-ide-token 使用裸 JWT；需要 Cloud-IDE-JWT 前缀的 Authorization 路径仍保留各自约定。
- 凭据列表保持掩码，完整值只在已存在的按需接口返回。测试和研究日志不写明文 token。

## 4. 每日签到、积分与计划任务

- 保留现有官方签到/状态端点、账号筛选、跳过规则、冷却和失败重试语义。
- Rust 和 Python 的客户端名称、app-version 与基础版本取自相同的原生客户端配置。
- 定时/无界面路径使用与桌面一致的凭据刷新逻辑和资源路径，不能依赖桌面旧 Work 正在运行。
- 通用积分、Work 积分的 product_id 分类保持；没有业务证据时不能把两类积分合并或重命名成另一类账本。
- 调研/自动化测试使用 fixture；真实签到作为单独运行结果报告，不把一次 HTTP 200 视为所有账号都成功。

## 5. 快照、切换、回滚与设备配置

- 统一客户端的主要快照根目录采用 data/profiles_trae。
- data/profiles 保留为兼容来源。迁移只复制缺失且有效的账号槽，不覆盖已有新版槽，不迁移 last、.bak、.seed-* 成为可选账号。
- 迁移使用临时目录与原子提交；失败不能留下被误认为完成的槽位。元数据记录来源和目标客户端。
- 完成前先备份当前真实登录态。恢复、切换和回滚都使用同一实际客户端与同一快照映射。
- 来源账号守卫使用关闭前确认的当前账号 ID；守卫不成立时不得覆盖来源账号槽。
- 仅复制现有核心登录文件，不用旧完整用户目录覆盖新版应用目录。
- 本次不改变设备映射的既有语义，不新增签到规避或配额规避机制。
- 精确识别主程序及子进程树，避免误杀 AI Work 助手或其他应用。
- PowerShell 桥支持解析后的 Trae CN 参数，保留旧 TargetApp 调用的兼容入口。

## 6. 项目接力、多实例与代理

- 前后端自动切换事件包含实际客户端目标，不能再次硬编码 TraeWork。
- 统一客户端启动支持现有代理参数和受控回环 CDP；不用正在运行的另一客户端的 CDP 页面接力。
- 项目列表、会话 ID、分享、任务恢复按新客户端的真实存储和 RPC 核对；没有可信会话或归属证据时给出明确失败。
- 多实例选择统一客户端 EXE，每个实例保留自己的 user-data-dir；只在用户原有显式选项启用时复制账号快照。
- 代理仍观察现有域名，新平台请求使用相同脱敏和错误分类。新 hub 路径按需要纳入诊断，不把它们自动视为生成或自动切号触发端点。

## 7. 网关、模型与媒体

- 对外 /v1/chat/completions、/v1/messages、/v1/responses、/v1/assets、/v1/videos/generations 等契约保持。
- 应用 ID、业务 function、版本和请求 token 格式集中管理；仍有效的 solo_work_lite 等值保留，不因品牌变化自动替换。
- 同步官方模型配置，保留 visibility/internal/custom 占位过滤与目录能力校验。
- 聊天、上传、视频任务、结果读取、预算与计费对账分别验证。仅因为安装包存在某端点不能宣称生成已适配。
- 不放宽 Core host lease、计费 fail-closed、幂等或 unknown 恢复约束。

## 8. 界面、文档和交付

环境配置、主页启动、账号发现、快照弹窗、帮助文案和设置统一展示“TRAE CN”，需要选择旧客户端时明确标为旧版。源码注释和文档中仍有效的历史、迁移兼容名称保留。

涉及的主要文件：

- Rust：新增原生客户端解析模块；commands/env、oauth、bitbrowser、accounts、trae_apps、switch、profile、proxy、relay、instances；native_snapshot、models、python、main；api_server 的常量与相关适配。
- Python：device_proxy、auto_checkin、trae_project_state，及新客户端配置共享辅助。
- PowerShell：trae-switch-bridge。
- React：store、types、lib/tauri、Dashboard、Accounts、Settings、TopBar、SetupGuide、账号相关弹窗和必要网关文案。
- 运维：计划任务运行路径、构建资源、项目速查与使用文档。

不自动升版本号、推送、合并或发布 GitHub Release。本地构建完成后，按已有可回滚部署机制替换当前运行资源，并报告实际激活结果。替换前保存原 EXE 和脚本资源。

## 验证设计

必须覆盖以下会影响账号数据的场景：

1. 只有旧 Work、只有统一 Trae CN、两者并存、旧配置路径已失效、迁移记录存在但目标安装缺失。
2. 旧目录保留且 UID 与新目录不同；当前 UID 判断必须选运行客户端。
3. 新旧快照同 UID 冲突、复制中断、已有新版槽、缺凭据槽、last/.bak 临时槽。
4. 新授权选择 TRAE client_id、存量刷新选择原签发 client_id、跨账号刷新结果拒绝、JWT 前缀适配。
5. 自动切号、显式切号、保存登录态、回滚、多实例和会话接力使用同一客户端。
6. 桌面/定时签到身份参数一致，已有设备、筛选、冷却和积分分类不变。
7. 网关路由与预算/计费契约保持；模型目录仍过滤内部模型。

测试使用隔离 fixture，不访问真实账号或运行中的服务；复用现有 Node/Rust/Python 工具链与 E 盘隔离 target，不下载工具链。真实本机检查与真实业务操作另外记录。

## 阶段与依赖

1. 统一客户端识别及兼容路由。
2. 注册、OAuth、JWT 与凭据归属。
3. 快照迁移、切换、账号发现、接力和多实例。
4. 签到、计划任务、代理与网关参数。
5. 界面文案、资源打包、必要回归、可回滚本地激活。

这些阶段共享客户端解析结果，按顺序实施。每阶段完成后记录文件、验证和未解决限制；不中断用户其他项目进程。

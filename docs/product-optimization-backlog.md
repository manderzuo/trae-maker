# 产品优化需求清单（全应用统一待办）

> **文档版本**: v2.6 · 2026-09-16
> **定位**: 全项目**唯一待办依据**——所有未实施的优化与需求项均在此登记，每条含需求概述 / 实现路径 / 验证依据。
> **v2.0 变更**: ① 合并删除五份分析文档——`docs/tmp/`（trae-account-switch-data-migration-analysis / doubao-api-feasibility / oss-ecosystem-value-analysis）、`work-credit-pool-design.md`（完整并入 §W-01）、`unified-api-gateway-design.md`（已实施，要点并入 tech-framework.md）；② WorkBuddy 蓝本（原 workbuddy-product-design.md）批次 1~5 已全部完成，其机会项 F-41/F-42/F-52/F-66 转入本文；③ 新增 F-67~F-73、E-01~E-03 共 10 项（源自上述分析文档中的未实现价值点）；④ 原 F-44（TRAE 多实例并行）改号 **F-67**，消除与 WorkBuddy 蓝本 F-44（会话备份，已完成）的编号冲突。
> **v2.1 变更**: 新增 **F-74 Trae OAuth 授权闭环补全**（源自 issue #10 用户反馈：OAuth 登录撞 SSL + 回调无监听，现状为"半实现"——详见条目）。
> **v2.2 变更**: 新增 **W-02 Seedance 视频生成 API 端点**（将 Trae Work CN 原生 Seedance 能力纳入本地 API 网关，现状为代理可转发但网关未提供视频路由）。
> **v2.3 变更**: 新增 **F-75 Trae 注册/登录辅助流程**（浏览器注册、Token/refreshToken 接入、实例数据目录隔离与设备标识语义核对；无痕模式不承诺规避服务端设备绑定）。
> **v2.4 变更**: F-75 补充比特浏览器窗口适配方案（Local API 定位窗口 + CDP 捕获登录响应；不读取密码，不回显 Local API Token）。
> **v2.5 变更**: BitBrowser 首次导入/原生凭据接管已落地；首次登录后自动生成 TRAE Work CN 快照，后续续期走本机 refresh_token，不绑定 BitBrowser 窗口 ID。
> **v2.6 变更**: F-67 首版落地独立 `--user-data-dir` 实例管理（创建/列表/启动/停止/移除登记），默认空目录，显式选择后才复制本机快照。
> **原则**: 接口层独立模块 + 失败明示 + 不硬编码奖励数额；仅管理本人合法持有的账号；借鉴开源遵循 learn-the-design, write-our-own-code。

> **本轮范围说明**：除 GPT Image 2.5 预留位外，主计划中的可实施项均已落地并完成代码烟测。表中“已评估暂缓/不实施”表示已完成技术与安全处置，不会偷偷启用未确认的豆包 Web 私有签名、第三方 IDE 注入或其他远期能力。

---

## 一、待办总览

| 编号 | 功能点 | 应用域 | 优先级 | 预估 | 状态 |
|---|---|---|---|---|---|
| F-68 | Trae 项目列表/最近打开跨账号保留 | Trae 生态 | **P1** | 1~2 天 | 已实现（切换桥已接入键级提取/合并；待最终统一验收） |
| F-74 | Trae OAuth 授权闭环补全（回环监听 + 代理豁免 + code 交换） | Trae 生态 | **P1** | 1~2 天（批次1）/ 3~4 天（全链路） | 已实现首版（回环监听、PKCE/code 交换、Trae OAuth 直连豁免；动态参数仍以最终验收为准） |
| F-75 | Trae 注册/登录辅助流程（清洁浏览器 + Token 接入 + 实例绑定） | Trae 生态 | **P1** | 2~3 天（先登录闭环） | 部分实现（BitBrowser 导入/UID 续期/注册引导已完成；验证码与风控仍需人工） |
| F-24-余 | 豆包会员额度端点抓包固化 | 豆包 | **P1** | 0.5~1 天（含抓包） | 框架已完成，仅剩前置 |
| F-38 | Trae → DSH 引导（不自研） | Trae 生态 | **P1** | ≈0（装即用） | 已实现（API 帮助内置安装说明与项目链接） |
| E-01 | 豆包对话网关（OpenAI 兼容 doubao provider） | 豆包/网关 | **P2** | 8~12 天（含 E-02） | 已评估暂缓：不属于 Trae Work CN Seedance 范围，依赖豆包 Web 私有签名与风控验证 |
| E-02 | 豆包指纹嗅探持久化 + a_bogus 纯算法生成器 | 豆包 | **P2** | 并入 E-01 批次 | 已评估暂缓：仅在明确纳入豆包 Web 通道且取得用户授权样本后实施 |
| W-01 | Work 积分（209）接入 API 网关（多活会话编排） | Trae/网关 | **P2** | 未定（运维重） | 首版已接入资源级 Work 取号（Seedance）；SOLO `create_agent_task` 多实例编排仍受原生客户端上下文约束 |
| W-02 | Seedance 视频生成 API 端点（原生请求转发 + 任务桥） | Trae/网关 | **P2** | 4~7 天（先文生视频） | 已完成首版（Work 积分取号、原生 SSE 任务桥、资源地址解析、参数校验、幂等键、账号级失败冷却与 SSE 前自动换号、跨重启任务索引、可配置视频缓存、Key 隔离查询与内容分发；真实动态设备头端到端需用户手动验收） |
| W-03 | Trae Work 桌面端额度耗尽自动切号（保留项目接力） | Trae Work | **P1** | 已实现首版（需真实额度耗尽验收） | 代理信号 + 账号池积分排序 + `switchAndContinue` |
| F-70 | Trae tc 凭证直读 + ECDSA P-256 刷新情报核对 | Trae 生态 | **P2** | 2~3 天 | 首版可用（tc 本地解密已接入账号发现；续期使用原生 ExchangeToken，不伪造 ECDSA 签名） |
| F-69 | Trae 会话导出存档（Markdown + 存档浏览器） | Trae 生态 | **P3** | 2~3 天 | 首版已完成（本机 API 会话列表/导出/删除/保留策略；云端 SOLO 会话端点不伪造） |
| E-03 | 豆包多模态端点（生图/生视频/音乐/文件中转站） | 豆包/网关 | **P3** | 3~4 天 | 已评估暂缓：依赖未纳入本轮的豆包 Web E-01/E-02 |
| F-67 | TRAE 多实例并行（原 F-44 改号） | Trae 生态 | P3 | 首版已完成 | 独立 `--user-data-dir` 实例管理已完成；实时原生会话编排仍待 Trae 官方/客户端入口 |
| F-07 | 豆包 cookie 级热切换（方案 B） | 豆包 | P3 | 1~2 天 | 已评估不实施：客户端二次加密没有安全、稳定的离线热切换接口 |
| F-41 | trae2codex 转换器 | Trae 生态 | P3 | 3 天 | 首版已完成（`/v1/responses` Trae SOLO 流式/聚合投影与会话接力） |
| F-42 | workbuddy-mcp 模式 | Buddy 生态 | P3 | 2~3 天 | 已评估暂缓：需用户明确 MCP 客户端、权限与凭证接入范围 |
| F-66 | CLI 多账号环境隔离 | Buddy 生态 | P3 | 评估先行 | 已评估暂缓：需先确认目标 CLI 的配置目录与并行运行需求 |
| F-52 | WorkBuddyProxy 模式（驾驶舱 + Codex 执行器） | Buddy 生态 | P3 | — | 已评估暂缓：与当前 Trae Work CN 主线无关，待明确需求后再立项 |
| F-71 | Trae SG 版（国际版）支持 | Trae 生态 | P3 | — | 已评估暂缓：本轮范围限定 Trae Work CN，未启用 SG 端点 |
| F-72 | 网关上游多级回退 + 分档竞速调度 | 网关 | P3 | — | 已评估暂缓：现有五态机、跨池回退与失败冷却已覆盖当前场景 |
| F-73 | 网关反哺 IDE（第三方模型进 Trae） | Trae/网关 | P3 | — | 已评估不实施：涉及 hosts/443 MITM 与第三方模型注入，需另行安全评审 |

> 已完成项不再列于此（F-13 到期日历 / F-43 CC Switch 协同等已在版本中落地，详见 CHANGELOG.md）。

---

## 二、条目详情

### F-68 Trae 项目列表/最近打开跨账号保留（P1）

- **需求概述**：切换账号后 Trae 内「项目列表」「最近打开」随槽位快照整体回滚而"消失"——根因是 `state.vscdb` 全局键（`solo-lite.local-project-folders`、`history.recentlyOpenedPathsList`）被快照覆盖，而项目本体（本地文件夹）与 `workspaceStorage`/`User/History` 本就跨账号保留。目标：**切到任何账号，项目列表与最近打开都在**。
- **数据归属事实**（2026-09-10 实测侦察结论）：`state.vscdb` 共约 200 键，其中 7 个账号前缀键（`solo-lite:content-map:<uid>` 会话映射、`solo-lite-mode-state-map-<uid>`）**按账号分区、绝不跨账号合并**（否则产生服务端归属校验失败的"幽灵会话"）；`local-project-folders` / `recentlyOpenedPathsList` 为**全局单键**，是本项目唯一可合并对象；登录态（storage.json/machineid）绝不合并。
- **实现路径**：
  1. 在 `src-ps/trae-switch-bridge.ps1` 的 `Switch` / `RestoreOnly` 管线中，恢复槽位快照**前**从当前 state.vscdb 抽出两个全局键，恢复**后**合并写回（`local-project-folders` 按项目 id 合并、快照内已有以快照为准；`recentlyOpenedPathsList` 去重并保留最近打开时间排序）；
  2. SQLite 键级读写：项目约束零新增依赖——优先 PS 调 Python `sqlite3`（标准库）小工具（`src-python/` 已有 sqlite 读库先例 `doubao_chats.py --check-login-cookie`），或 PS `System.Data.SQLite`（系统未必自带，需探测）；
  3. 操作前对 `state.vscdb` 做一次性 `.bak` 备份，失败回滚；全程在 Trae 未运行窗口期执行（切换流程本就先关闭，天然满足）。
- **验证依据**：无直接同类实现；SQLite 处理沿用本项目 `doubao_chats.py` 既有模式。
- **验收**：双账号各建若干项目后互切，项目列表与最近打开完整保留；账号分区键零改动。

### F-74 Trae OAuth 授权闭环补全（P1，首版已完成——源自 issue #10）

- **背景（issue #10，2026-09-13）**：用户走 OAuth 登录报 `ERR_CERT_AUTHORITY_INVALID`（www.trae.cn）且回调无法到达，WorkBuddy 侧正常。根因有二：① 本软件 MITM 代理运行时会把系统代理指向 `127.0.0.1:8899`，浏览器访问 OAuth 登录页被解密，自签 CA 未被信任即撞 SSL；② redirect_uri 指向的 `127.0.0.1:17388` **本机没有任何进程在监听**，浏览器跳转后只是"无法访问"页，需用户手动复制地址栏 URL 粘贴回来——链路从未真正闭环。
- **现状盘点（代码已实现的部分，勿重复造）**：`src-tauri/src/commands/oauth.rs` 已有 `oauth_get_login_url`（state CSRF + machine_id/device_id 生成）、`oauth_parse_callback`（宽容字段解析）、`exchange_token`（`api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken`）、`get_user_info`、`oauth_login`（vault 加密落库 + 分组）；`accounts.rs::refresh_jwt_impl` 已有 refresh_token → 新 JWT 续期（含冷却自动解冻）；前端 `OAuthLoginModal.tsx` 三步向导（打开登录页 → **手动粘贴回调 URL** → 落库）。Buddy 侧另有完整先例可对照（`workbuddy_oauth_login`：后端开浏览器 + 轮询 + 自动入池，F-50）。
- **首版实现内容**：
  1. OAuth 期间在本机回环端口启动短生命周期监听器，支持标准 `code` + PKCE 交换，并保留手动粘贴回调兜底；
  2. `device_proxy.py` 对 `trae.cn` / `trae.com.cn` 走原始 TLS 直连，避免系统 MITM 代理造成登录页证书错误；Trae API 上游仍按原有策略处理；
  3. 登录结果只落入加密账号存储，设备标识沿用本机映射；ExchangeToken 参数保持服务端接受的现有形态，不伪造 ECDSA 签名。
- **验证依据**：本项目 Buddy 侧 `workbuddy_oauth_login`（后端开浏览器 + 轮询 + 自动入池）与本地 OAuth 回调测试；Token 刷新参数以服务端响应和本地抓包结果为准。
- **验收**：MITM 代理运行中（复现 issue #10 环境）发起 OAuth 登录 → 浏览器完成授权 → 应用自动弹出“账号已添加”，全程无需手动复制 URL；粘贴回调 URL 兜底路径保留可用；登录页不再出现证书告警；OAuth 账号的签到/续期与 MITM 捕获账号行为一致。

### F-75 Trae 注册/登录辅助流程（P1，注册需人工完成验证）

- **需求概述**：补齐 Trae Work CN 账号的“注册 → 登录 → 纳入账号池 → 绑定独立实例”流程。注册与验证码/风控验证由用户在浏览器中手动完成；应用负责在登录成功后安全接收 Token、refreshToken、用户资料并落库，随后为账号创建/绑定独立 `--user-data-dir`。本项目标是账号隔离与可恢复登录，不承诺或设计为规避平台的设备绑定、风控或注册限制。
- **现状与代码依据**：浏览器登录窗口使用 Tauri WebView `incognito(true)`，注入脚本仅监听 `GetUserToken` / `GetUserInfo` 并回调本机服务；账号记录保存 JWT、可选 refreshToken 和来源标记。真正启动 Work 实例时，`--user-data-dir` 下会写入 `machineid`，并由其派生 `telemetry.machineId`，因此“无痕浏览器”只隔离浏览器会话，不等同于服务端不会识别或绑定设备。
- **实现路径**：
  1. 注册入口仅提供“打开清洁/无痕浏览器”与回到应用继续登录的引导；不自动填写验证码、不绕过 CAPTCHA/短信/风控；
  2. 登录接入复用现有浏览器 Token 捕获协议，增加 refreshToken 缺失/轮换提示、Token 过期检测和失败重试；敏感值只进加密存储，日志脱敏；
  3. 登录成功后先以占位账号入池，用户选择实例后再写入独立 data-dir；记录 `user_id`、实例目录、产品 machineid 的关系，避免复用正在使用的账号目录；
  4. 提供“登录态检查/重新登录/解绑实例”三种可恢复操作；设备标识语义以服务端返回和本地实际文件为准，不把随机 machineid 当成“未绑定”证明；
  5. 与 F-74 OAuth 回环监听共用登录结果事件，但保留手动粘贴回调/手动 Token 作为故障兜底。
  6. **BitBrowser 适配（已实现）**：扫描/导入通过 Local API 定位 profile，再用 `/browser/open` 返回的 CDP `ws/http` 地址读取页面 localStorage 登录态；首次导入会自动尝试一次原生 OAuth 接管，生成 icube 快照并把 refresh_token 加密入池。后续刷新/续期走本机 Trae Work CN `ExchangeToken`，不再依赖 BitBrowser。密码、验证码、Local API Token 和完整 Cookie 不进入日志；Cookie 仅在上游确实需要且用户再次确认时作为最小化、临时凭证使用。
- **验收**：用户在清洁浏览器中完成一次合法注册和登录后，应用能自动或经用户确认纳入账号池；新实例启动后只使用该账号的数据目录；重启/Token 过期可恢复；日志与导出文件不泄露 JWT、Cookie 或验证码；关闭代理或更换浏览器后仍能明确显示登录/设备绑定状态。
- **合规边界**：仅处理用户本人合法持有的账号；不实现批量注册、验证码/CAPTCHA 绕过、设备指纹伪装或规避平台限制。
- **验证依据**：本项目 F-74（OAuth 回环与代理豁免）及本地浏览器登录捕获、`storage.json` 读写和实例隔离测试。

### F-24-余 豆包会员额度端点抓包固化（P1，框架已完成）

- **需求概述**：豆包会员额度（套餐/到期/赠送额度）展示框架已就绪，仅剩把会员额度 XHR 端点经 MITM 抓包固化。
- **实现路径**：`device_proxy.py` 开启 + `open_doubao_app(proxyPort)` 注入 `--proxy-server` 拉起豆包客户端 → 会员页触发额度请求 → 抓包关键词 `membership|entitlement|quota|remaining|benefit` 定位端点 → 填入 `settings.doubao_quota_url` 即用。
- **验证依据**：端点为豆包私有；抓包链路复用本项目 MITM 基建并以本地回归测试为准。

### F-38 Trae → DSH 引导（P1，不自研）

- **需求概述**：不在本项目内复制 DSH 宿主；应用内提供本地网关地址、工具配置和故障排查说明，使 Trae 模型、账号切换与积分只读面板可以按用户环境接入。
- **实现路径**：Trae 侧新增引导卡片（安装步骤 + 常见问题）；产品化时沿用本项目 `storage.json` 发现与 loopback shim 的安全边界。
- **验证依据**：本地 DSH/MCP 配置、健康检查和视频任务轮询测试。

### E-01 豆包对话网关——OpenAI 兼容 doubao provider（P2）

- **需求概述**：把豆包 Web 端对话能力（`POST www.doubao.com/samantha/chat/completion`）接入现有 axum 统一网关，成为与 trae/buddy/custom 并列的第四类资源池；对外暴露 `/v1/chat/completions`，多轮对话（`conversation_id` 映射表为主、消息合并兜底）、三模式（`doubao` 快速 / `doubao-think` 思考 / `doubao-expert` 专家 → `completion_option` 参数组）、思考链映射 OpenAI `reasoning_content`。风控为「验证码墙」而非拒绝服务（`710022004` → `needs_captcha`，人工过后恢复），失败模式可探测可降级。
- **实现路径**（方案 B：MITM 嗅探 + 纯算法签名，零新增外部运行时依赖）：
  1. **批次 0 探测实验（0.5~1 天，先决）**：抓一次真实对话黄金样本 → Python 重放四档签名组合（随机/真实 msToken+随机 a_bogus/真实+算法 a_bogus/原样）→ 得出风控容忍矩阵，决定签名档位；
  2. **批次 2**：Rust 移植 SM3 + RC4 + s4 自定义 base64（约 300 行，不引入新 crate），以黄金样本做同参同 UA 输出比对单测；axum 网关新增 doubao provider（Cookie 组装 `sessionid+msToken+ttwid`、FAKE_HEADERS 从真实流量采样、payload 构造、SSE→OpenAI 转换复用现有转换层、conversation_id 映射表）；
  3. **错误矩阵**：`710012001` sessionid 吊销 → 标记失效停止调度（复用探活逻辑）；`710022004` → 账号冷却 + `needs_captcha` 状态；HTTP 200 无数据流 → 计入连续失败退避升级；
  4. 反封号组合拳（限速 + 随机延迟 + 指数退避，UA 保持真实采样值）。
- **验证依据**：以本地 MITM 样本、官方响应结构和协议单元测试为准；不把第三方实现、代码或密钥纳入运行时依赖。
- **验收**：OpenAI SDK 以 `base_url=http://127.0.0.1:<port>/v1` 完成流式多轮对话（快速/思考两模式）；退出某账号登录后网关 60s 内标记失效；重启应用无需重新抓包（指纹从库加载）。

### E-02 豆包指纹嗅探持久化 + a_bogus 生成器（P2，E-01 前置）

- **需求概述**：E-01 的基建前置。现有 MITM 代理扩展豆包域名过滤器，把 `msToken`（URL query + Cookie 双处）、`ttwid`/`passport_csrf_token`、`device_id`/`web_id`/`tea_uuid`（19 位设备指纹）按账号维度落库（`doubao_fingerprint`，带 `captured_at` 新鲜度）。**关键边界**：a_bogus 绑定单次请求（query + UA + 时间戳嵌入签名体），嗅探只能固定 payload 短窗重放，**必须纯算法生成**（SM3 双哈希 + RC4 固定 keystream + s4 base64，192 字符）；设备指纹必须与账号绑定且保持一致，频繁更换 device_id 是风控高危信号。
- **实现路径**：`device_proxy.py` 嗅探器扩展（与 sessionid 抓包同库同账号存储）→ 管理页展示指纹新鲜度（无指纹账号标记"未经代理采集"）→ Rust a_bogus 生成器（见 E-01 批次 2）。技术情报详见 tech-framework.md 附录 C。
- **验证依据**：同 E-01；以本地抓包样本、设备指纹一致性测试和回放结果为准。

### W-01 Work 积分（209）接入 API 网关（P2，方案已论证 + 有实现可抄）

> 原独立设计文档 `work-credit-pool-design.md` 已完整并入本节（2026-09-13）。详见 §三。

### F-70 Trae tc 凭证直读 + ECDSA P-256 刷新情报核对（P2）

- **需求概述**：① Trae CN 的 `storage.json` 凭证使用自定义 "tc" 加密 = **AES-128-CBC + SHA-512**（SG 版为明文 JSON）——实现直读解密后，本机 Trae 凭证发现不再依赖 MITM 抓包；② Token 刷新所需的加密参数、端点和签名字段以本地样本及服务端响应核对，作为 `refresh_jwt` 续期链路的底层依据。
- **实现路径**：
  1. 先用本地样本做协议核对（解密参数、ECDSA 签名细节及与积分/套餐/会话相关的端点）；
  2. `jwt.rs` / `trae_apps.rs` 增加 tc 解密读取路径（Rust 实现 AES-128-CBC，`aes`/`cbc` crate 需评估零新增依赖红线——必要时经 Python `cryptography` 旁路，项目已依赖）；
  3. 与现有 MITM 捕获路径并存（解密成功优先，失败回退抓包），`apps_accounts_discover` 账号发现覆盖面扩大。
- **验证依据**：本地 `storage.json` 样本、Trae CN/SG 实际响应和现有 Tauri 账号发现测试；不依赖第三方仓库。
- **风险**：解密实现属逆向范畴，仅读本机自有凭证；接口变更由 dig() 宽容解析兜底。

### F-69 Trae 会话导出存档（P3）

- **需求概述**：用旧账号 JWT 调 SOLO 会话接口导出对话内容为 Markdown，按账号归档到助手数据目录，前端提供存档浏览器（按账号/日期/项目筛选）。不改变云端数据归属，零风控风险。**边界**：仅"存档"，新账号下不能继续对话——会话真迁移（场景 C）已被服务端 `user_id` 归属校验证伪，见 §四已排除项。
- **实现路径**：
  1. 抓包确认 SOLO 会话列表/详情接口（列表 + 消息体结构，工具调用/文件引用的形态）；
  2. `src-python/` 新增导出脚本（复用 `doubao_export_chats` 的 IM 接口导出模式：分页拉取 → Markdown + JSON 双格式落 `data/exports/trae_chats_<uid>_<ts>`）；
  3. 前端存档浏览器（复用豆包对话导出的交互形态）。
- **验证依据**：本项目 `doubao_export_chats`（同族先例，交互与导出格式直接复用）及本地分页回归测试。

### E-03 豆包多模态端点（P3，依赖 E-01）

- **需求概述**：E-01 之上的豆包多模态能力暴露：① **生图** `/v1/images/generations`——同端点意图路由，SSE `block_type=2074` 的 `creations[]`，`image.status==2` 完成，取 URL 优先级 `image_ori > image_raw > thumb`（**image_ori 通常无水印**）；SSE 漏图时轮询 `/message_node_info` 兜底；图生图先上传参考图得 `ref_image_key`；② **生视频** `/v1/video/generations`——两步异步（`content_type=2020` 下发 → `fin_reason.async_task.id` → `/samantha/chat/async/stream` SSE 长连接等 `2021`，1~3 分钟），需任务桥表 + 中断重连（event_id 游标）+ 账号级并发上限；③ **音乐**——同端点同步返回（30~60s）；④ **文件中转站** `/v1/files`——TOS 上传 ≤1GB 得永久 URI（免费跨机文件通道，顺带收益）。
- **实现路径**：E-01 批次 3 照原方案实施（任务桥表 `task_id ↔ 账号 ↔ 状态`、超时重连、多模态 bot_id `7338286299411103781` 路由、图片理解需先 TOS 上传）。识图/文档理解（60+ 格式）一并获得。
- **验证依据**：本地 SSE 样本、`message_node_info` 兜底回归测试与网关端点契约；不复制第三方实现。
- **边界**：**去水印仅指获取平台自有 image_ori 原图**；已烘焙进画面的 AI 水印属图像内容，网关不去除（TickClear 工具线范畴），见 §四。

### F-67 TRAE 多实例并行（P3，原 F-44，首版已完成）

- **需求概述**：每账号独立 `--user-data-dir` 启动多个 TRAE 实例并行运行，账号轮换不再依赖「关闭 → 快照恢复 → 重启」单实例管线，从根本上规避快照白名单随 TRAE 版本漂移失效的问题（历史测试表明，只有独立实例隔离能稳定保留登录态）。
- **实现路径**：`src-tauri/src/commands/instances.rs` 提供实例注册表与目录安全校验；设置页提供创建、列表、启动、停止、移除登记。启动时调用已探测的 Trae Work 可执行文件并传入独立 `--user-data-dir`，代理运行时可注入本机代理端口。默认创建空目录，只有用户显式勾选“使用所选账号已有快照”才复制本机快照；移除登记保留数据目录，避免误删登录态与项目数据。原有单实例切换管线继续作为兼容回退。
- **已知边界**：实例目录隔离不等于服务端设备绑定解除；空目录实例需要用户在 Trae Work 内完成登录，种子快照只复制本机已有状态。Trae 本身若限制单进程或要求动态设备头，助手不会伪造绕过，需回退到原生切换管线。
- **验证依据**：本项目实例注册表、目录安全校验与 `--user-data-dir` 回归测试；不依赖第三方实现。
- **验收**：至少两个账号同时在线使用互不干扰；与 W-01 的 N 实例运维模型天然互补（同一底座）。

### F-07 豆包 cookie 级热切换（P3，方案 B）

- **需求概述**：不重启客户端的进程内账号热切换——读取 Cookies 表 → DPAPI 解密 → 账号池管理 → 重写 Cookies 行重加密写回。
- **实现路径**：**先验证再开发**——实测豆包客户端 cookie 在 DPAPI 之下还有一层客户端级二次加密（明文为二进制密文），离线拿不到明文 sessionid；sessionid 池化需先验证网页版 cookie 通道可行。E-02 指纹嗅探落库后可复用其凭证管理底座。
- **验证依据**：无成熟同类；Chromium Cookies DPAPI 结构处理以公开协议资料和本地样本为准。

### F-41 trae2codex 转换器（P3，首版已完成）

- **需求概述**：Trae 上游为自有 `llm_utils_chat` 协议、无 Responses API，Codex CLI 不能直连；自建转换层把 Codex `/v1/responses` 请求投影到 Trae SOLO 上游——社区空白机会。
- **实现路径**：复用网关已有 WB 侧投影逻辑，`responses_api` 已按统一调度分流到 Trae/WB/Custom；SOLO SSE 侧增加 `Protocol::Responses` 生命周期事件转换，聚合与流式均保留本地会话键与 UID 分区；Codex CLI 可直接把网关 `/v1` 配为 `responses`。
- **验证依据**：本项目 `wb_responses.rs`（投影逻辑直接复用）与 Responses 协议单元测试。

### F-42 workbuddy-mcp 模式（P3，机会项）

- **需求概述**：把 WorkBuddy 注册为 Codex / Claude Code / Cursor 的 MCP 工具（Model Context Protocol server），使这些客户端经 MCP 调用 Buddy 网关能力（模型对话、积分查询、账号状态）。
- **实现路径**：网关侧新增 stdio MCP server 入口（JSON-RPC 2.0，tools 暴露 chat/credits/status）；`WB_SKIP_PERMISSIONS` 权限可控；与 ck_ 子 Key 体系打通（子 Key 即 MCP 凭证）。
- **验证依据**：MCP 官方规范与本项目 stdio JSON-RPC 单元测试。

### F-66 CLI 多账号环境隔离（P3，机会项，评估先行）

- **需求概述**：每账号独立 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` / `KIMI_CODE_HOME` 环境目录 + 全局同名变量剥离 + 「严格账号模式」（无激活账号即报错、不回落本机登录态）+ 接口返回一律脱敏——与 F-06 CLI 切号桥互补（写 token vs 隔目录），覆盖 dsh/CC 多 CLI 并行场景。
- **实现路径**：先做评估（目标 CLI 的配置目录读取优先级、与现有 `workbuddy_cli_bridge_set` 写 token 模式的冲突调和），通过后作为 CLI 桥的第二种隔离模式并存。
- **验证依据**：本项目多 CLI 账号环境隔离、严格账号模式与接口脱敏测试。

### F-52 WorkBuddyProxy 模式（P3，远期）

- **需求概述**：WorkBuddy 驾驶舱 + Codex 执行器——与 F-40（Codex 协议投影进 Buddy 网关）方向相反：以 WorkBuddy 客户端为主控、Codex 作为执行后端。
- **实现路径**：远期评估，暂无排期；待 F-41 / F-42 落地后按生态需求决定。
- **验证依据**：本项目多应用统一网关的资源池抽象与协议兼容测试。

### F-71 Trae SG 版（国际版）支持（P3，远期）

- **需求概述**：支持 Trae SG / SOLO SG（国际版）：SG 版 `storage.json` 为**明文 JSON**（无 tc 加密），端点 `a0ai-api-sg.byteintlapi.com`，SOLO 与主版共用 chat 端点仅认证路径不同；CN/SG SSE 格式有差异（CN 每条 data 前有 `event:output` 前缀，SG 无，需自适应解析）。
- **实现路径**：账号发现增加 SG 档案（`app_locate` 扩展）→ JWT/端点路由按区域分流 → `sse.rs` 解析器兼容两种格式；前置情报已由开源实现验证，实施前拉最新源码核对。
- **验证依据**：本地 CN/SG 认证样本、端点路由表和协议回归测试。

### F-72 网关上游多级回退 + 分档竞速调度（P3，远期）

- **需求概述**：① 上游端点故障自动降级尝试（3 级端点回退）；② 按模型能力分 5 档、同档并发竞速、排队过长自动降档；③ 检测图片输入自动切多模态模型——网关可用性与延迟的增强方向，与现有会话粘性/五态机互补。
- **实现路径**：远期；现有五态机 + 分级重试 + 池间回退已覆盖主要故障形态，本项在多上游（E-01 落地后豆包+Trae+Buddy 三池）场景收益才显著。
- **验证依据**：本项目 3 级回退、分档竞速和多模态切换的集成测试。

### F-73 网关反哺 IDE（第三方模型进 Trae）（P3，远期留档）

- **需求概述**：反向思路——让 Trae IDE 本体调用第三方模型 API（百炼 / Kimi Coding Plan 等）：hosts 劫持 `api.openai.com` → 127.0.0.1 + 443 本地反代 + CA 证书，本地伪 `/v1/models`，智能路径转换 `/v1→/v2`，多服务商配置一键切换。
- **实现路径**：远期留档；本项目 MITM 体系已覆盖同类能力（更通用），但"多服务商配置 + 一键切换激活"的交互值得借鉴；待用户需求明确再评估。
- **验证依据**：本项目代理、反向隧道和 TLS 配置的隔离测试。

### W-02 Seedance 视频生成 API 端点（P2，代理链路已验证）

- **需求概述**：将 Trae Work CN 内置 Seedance 能力纳入本地 API 网关，提供标准化的视频生成请求入口；第一阶段支持文生视频，后续补充图生视频、任务查询、结果下载与多账号调度。
- **当前现状**：本地 MITM 代理已能观察并稳定转发原生 `POST /api/ide/v1/tool_text_to_video_stream` SSE 请求；API 网关已提供 `POST /v1/videos/generations`、`GET /v1/videos/:task_id` 与 `GET /v1/videos/:task_id/content` 异步任务桥，按 Work 积分取号、持久化任务索引并缓存视频产物。新增 `POST /v1/assets` 临时素材仓、魔数校验、API Key 隔离、过期清理和短时内容链接；MCP 支持调用方本地 `image_paths` / `video_paths`、拖拽 Base64/data URL 或附件对象上传后以 `image_asset_ids` / `video_asset_ids` 引用。素材公开基址已可在网关设置页或环境变量显式配置；原生插件仍保留作为动态设备头不兼容时的降级入口。
- **实现路径**：已新增 `POST /v1/videos/generations`（兼容查询端点）与任务状态查询；Seedance transport 复用 Trae 账号池 JWT / 设备映射，处理长连接 SSE、资源 URL 解析、幂等任务键与失败冷却。参考素材可先存入本地素材仓；显式设置 `AIWORK_ASSET_PUBLIC_BASE_URL` 后，资产 ID 转换为 Trae 可回取的随机短时链接。原生动态设备头（`x-helios` / `x-medusa` / `x-neptune`）由 Trae 客户端生成，若上游要求动态签名则保持任务失败明示并回退原生插件，不伪造签名。视频额度按 Trae Work 原生口径单独记账。
- **安全与合规约束**：仅管理用户本人合法持有的 Trae Work 账号；不在日志中记录 JWT、动态签名或完整视频请求体；保留原生插件路径作为降级入口。实现前先用单账号、短时长文生视频做端到端验收。
- **验收**：外部客户端提交一次标准 JSON → 返回任务 ID / 可选 SSE；任务完成后可获取视频地址或本地文件；参考图/参考视频仅在用户显式提交且配置可达素材基址时传递；上游失败不重复扣费、不重复发起任务；账号与额度归属可追溯；原生 Trae Work Seedance 功能不受影响。Trae 原生参考图上传协议仍待一次真实请求确认，未确认前不猜测上游上传接口。
- **参考实现**：本项目 `src-python/device_proxy.py` 的 Seedance SSE 透传与日志分类；API 网关 `src-tauri/src/api_server/routes.rs`、`pool.rs`、`sse.rs` 的调度 / 流式输出基础设施。

---

## 三、W-01 Work 积分接入网关（专题，完整吸收原 work-credit-pool-design.md）

### 3.1 背景与现状

现有文字 `llm_utils_chat` 通道仍按 **IDE/通用积分（product_id 208）** 取号；Work 积分（209）已作为独立资源类型接入 Seedance 原生任务桥，避免把 Work-only 账号误放行到文字通道。

### 3.2 关键约束（为什么外部无法复刻）

- **实证**：真实 Trae SOLO 客户端发起 `create_agent_task` 返回 200 + SSE（`task_created` / `model_config`），确认成功消耗 Work 积分；请求体由原生层 `ai_agent.dll` 构造（~123KB 富上下文），闭源 `@aha-kit` 加密（仅暴露 `init`/`rawFetch`），body 加密后无法直读。
- **复刻证伪**：用真实身份（真实 JWT `data.id` + 真实 `device_id` + `machine_id` + `project_id`）复刻 → 仍返回 `4001 failed to get summary template data`。**`create_agent_task` 必须由实时 Trae SOLO 会话自身发起**；`@aha-kit` fetch 走 TTNet 隧道（MITM 只见 CONNECT 中继），真实客户端是直连 HTTPS + aha 加密体，两条路径不同。

### 3.3 可行方案结论

| 方案 | 结论 |
|---|---|
| **A. 多活会话编排 + work_transport（推荐）** | 每 Work 账号跑一个独立 `--user-data-dir` 的实时 Trae SOLO 实例，工具作编排层：按池选账号 → 驱动对应实例发起 `create_agent_task` → MITM 捕获 SSE → `sse.rs` 转 OpenAI。当前已先交付 Work 资源取号与 Seedance 任务桥，多活 SOLO 对话仍需原生客户端 IPC/自动化入口。★★★★★ |
| B. 复用 IDE 池过渡 | 零改动跑通形态，但消耗 IDE 积分不满足诉求（仅过渡，形态已由现有网关验证完毕） |
| C. 逆向原生层复刻 | 已证伪（4001）+ 闭源逆向合规风险，放弃 |
| D. 纯 MITM 中继 | 只能观察不能发起，不作主方案（调试价值保留） |

**实现要点**：① `pool.rs` 已提供 `ResourceKind::Work` 独立取号；② `/v1/videos/generations` 已以 Work 账号驱动 Trae 原生 Seedance SSE；③ 文字通道继续显式使用 General，Work-only 账号不会被误选；④ `create_agent_task` 仍必须由实时 Trae SOLO 自身构造，当前不伪造加密富上下文体，原生插件作为降级入口。

### 3.4 生态佐证（2026-09-11 调研，W-01 升 P2 依据）

本地协议样本与两套独立回归实现交叉验证同一通道：**`llm_utils_chat + function=solo_work_lite`**（SOLO 免费对话通道，队列比 Trae CN 主通道轻）——W-01 从"方案已论证"进入"**已有本地验证**"阶段。Buddy 网关批次 2 改造完成后，其调度/熔断/协议输出层可直接复用；主服务与辅助进程分离、healthcheck 常驻的工程形态可作桌面端内置网关的拆分参照。

### 3.5 开发前必须解决的未决项

| # | 未决项 | 说明 |
|---|---|---|
| 1 | 如何"驱动"实时 SOLO 会话发起 `create_agent_task` | 优先确认本地命令/IPC/扩展 API；退化 headless/UI 自动化（脆弱，仅兜底）。F-67 多实例底座落地后此问题简化 |
| 2 | Work 积分余额 API 来源 | 与 IDE 的 `ide_user_ent_usage` 不同，端点/字段/鉴权需继续通过本地抓包与服务端响应核对 |
| 3 | 单实例多账号可行性 | 若 `create_agent_task` 强绑定登录会话则必须 N 实例；进程内切号可大幅降运维成本，需实验确认 |

**风险**：驱动真实客户端批量消耗 Work 积分可能触及 Trae ToS，上线前需评估；N 实例资源占用/登录态维护/崩溃恢复（`pool.rs` 的 `SessionDead` 冷却机制天然适配）。

---

## 四、已排除项（明确不做，留档防重复提出）

| 项 | 排除原因 |
|---|---|
| Trae 会话真迁移/复制回放（场景 C） | 服务端按 `user_id` 归属校验拒绝（"幽灵会话"）；真迁移需以目标账号身份重建会话回放消息，非公开接口 + 风控风险 + 版本易碎——以 F-69 导出存档替代 |
| Trae state.vscdb 账号分区键跨账号合并 | 产生服务端归属校验失败的"幽灵会话"（F-68 实现红线） |
| 豆包生成图内容级去水印 | 已烘焙进画面的 AI 水印属图像内容，需 inpainting 类后处理——TickClear 工具线范畴，不在网关承诺（E-03 仅交付 image_ori 原图 URL） |
| E1 随机签名方案（方案 A） | 历史随机签名方案，2026 年风控下大概率失效；仅作探测实验的对照组 |
| E1 浏览器签名方案（方案 C，Playwright） | 最稳但引入 Chromium 常驻运行时，与零新增依赖红线冲突；保留为风控升级后的 Plan B |
| GitHub Actions 免常驻签到 | 与桌面端产品定位不符（Maquer/trae-signin 模式） |
| 多通道网关统一接入（QClaw/QwenWork 等） | 现有协议兼容层已验证方向，但当前无用户诉求，远期再议 |
| F-19 失败通知渠道扩展（企业微信/Server酱/Bark webhook） | 用户明确不做（已从待办移除；注意：WorkBuddy 蓝本 F-19 曾在企业微信/Server酱 上落地过 T3.6，豆包/Trae 侧不做） |
| 日志导出（CSV/文件导出） | 用户明确不做（T6 设计时明确排除） |
| `/v1/embeddings` 端点 | 上游无对应能力，明确返回 501，不做假实现 |
| C 方案：逆向原生层复刻 `create_agent_task` | 已证伪（4001）+ 闭源逆向合规风险（W-01 §3.3） |
| 账号池调度权重/时段轮询（T10 裁剪） | 避免过度设计 |

---

## 五、建议排序

1. **F-68 项目列表跨账号保留** —— 1~2 天，切换体验的显性痛点，方案已论证零风险
2. **F-74 OAuth 授权闭环补全** —— 批次 1+2 约 2 天，已有用户卡在此处（issue #10），批次 3 与 F-24-余 抓包同批做
3. **F-75 注册/登录辅助流程** —— 先做登录闭环与实例绑定；注册验证保持人工完成，不承诺规避设备绑定
4. **F-24-余 豆包额度端点固化** —— 半天抓包点亮已建好的框架
5. **F-38 DSH 引导页** —— 成本≈0，随手带上
6. **E-01/E-02 豆包网关** —— 已评估暂缓，本轮聚焦 Trae Work CN Seedance
7. **W-01 Work 积分接入** —— Seedance Work 取号已完成；SOLO `create_agent_task` 仍受原生客户端上下文约束
8. **F-70 tc 凭证直读** —— tc 本地解密已接入；续期使用原生 ExchangeToken，不伪造签名
9. F-69 / E-03 / F-41 / F-42 / F-66 —— 已完成或已评估暂缓，按需重新立项
10. F-52 / F-71 / F-72 / F-73 —— 已完成范围评估，后续需单独授权/安全评审

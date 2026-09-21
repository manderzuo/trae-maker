# AI Work 网关部署准备

## 当前边界

桌面版负责 Windows 专属能力：Trae/BitBrowser 登录、原生快照、设备切换和 JWT
捕获。API 网关、账号池调度和 Seedance 任务桥已经把数据根目录、监听地址、端口和
视频目录解耦，但当前 Tauri 进程仍是桌面控制面，不应直接当作 Linux 无头服务运行。

## 运行时覆盖

```text
AIWORK_DATA_DIR=/srv/aiwork/data
AIWORK_VIDEO_DIR=/srv/aiwork/videos
AIWORK_ASSET_DIR=/srv/aiwork/assets
AIWORK_BIND=0.0.0.0
AIWORK_PORT=7864
AIWORK_PUBLIC_BASE_URL=https://api.example.com
AIWORK_ASSET_PUBLIC_BASE_URL=https://api.example.com/v1
AIWORK_DEFAULT_MODEL=deepseek-v4-flash
AIWORK_CORS_ORIGINS=https://console.example.com
```

桌面端未设置这些变量时，仍使用 `%APPDATA%\\AIWorkAssistant` 和配置页中的值。
变量只覆盖运行时配置，不改写用户的登录文件或 SSH 私钥。

## 星链维度分流系统管理员登录

独立 Core 管理页面地址：本机为 `http://127.0.0.1:7865/admin`，公网为
`https://api.gemstory.cn/admin`。页面使用 `admin` 账户登录和浏览器会话，不再要求在页面中
填写或保存 Core 管理员 API Key；普通用户 API Key 仍在管理页面中创建并按作用域、积分和并发
限制使用。

首次启动前，在服务进程的运行环境中设置 `STARLINK_ADMIN_INITIAL_PASSWORD`。该值只用于创建
初始管理员凭证，首次登录后必须修改；不要把它写入仓库、启动脚本、网页、日志或反向代理配置。
如果已有管理员凭证，环境变量不会覆盖现有密码。忘记密码时应停止服务后按既定运维流程恢复凭证，
不要在公网打开未认证的初始化接口。

公网只允许通过 HTTPS 反向代理访问 `/admin` 和 `/v1/*`，并设置安全 Cookie 转发；不要直接
暴露 Core 的 7865 端口。反向代理只负责 TLS 和转发，不负责账户、Key、积分或并发授权。

## Core 基础模式与数据库运维（Phase 0/1）

网关设置 `core_mode` 默认为 `off`，可切换为 `shadow` 或 `enforce`。`off` 保持现有
legacy 鉴权/请求路径；`shadow` 只做 Core 身份和迁移观察，不用 Core 结果拒绝请求；
`enforce` 才以 Core Key、scope、cost policy、逻辑额度和幂等结果为权威。Phase 1 的
`enforce` 仅支持非流式 Chat，`stream=true` 返回 501；视频、素材、真实账号调度和
重启对账不属于本阶段。

Core SQLite 位于 `<AIWORK_DATA_DIR>\\data\\core.sqlite3`，未设置变量时为
`%APPDATA%\\AIWorkAssistant\\data\\core.sqlite3`。备份必须先停止 API 服务和桌面应用，
再复制整个 `data` 目录（包含存在的 `core.sqlite3-wal`/`core.sqlite3-shm`），并保留旧
JSON 和迁移报告；禁止对正在运行的 SQLite 做热文件复制。

迁移前必须先做可恢复备份。使用 `core_migration_inspect` 生成报告，读取并保留待核对的 owner
字段（不做 owner 映射校验，也不返回候选映射；真正的 owner 校验在 apply 阶段执行）；inspect
仅检查 JSON 可解析性、数量和哈希；owner 是否存在以及素材文件存在性、大小和 SHA 由
`core_migration_apply` 做 fail-closed 校验。写入/变更类命令
`core_migration_apply`、`core_user_create`、`core_api_key_issue` 和 `core_quota_grant`
必须提供已认证的管理员会话；旧的 admin API Key 仅保留给兼容自动化路径；只读的
`core_status`、`core_migration_inspect` 是例外，具体鉴权以当前实现为准。不能由客户端传入 actor 身份。Key 明文只在签发响应中显示一次，
Core 只保存 digest/prefix。

切换 `enforce` 前必须完成：迁移前备份、user、Key、scope、cost policy、grant 和 parity
report。旧 JSON 继续保留，不会被当作 Core grant 或真实上游余额。Phase 1 smoke 只使用
Mock executor；不得以真实 upstream、真实余额、真实计费或真实生成结果作为本阶段部署
验证证据。

## Core schema v12 迁移与双层预算运维

本版本的 v11→v12 是单事务迁移：新增 `quota_budget_accounts` 并把旧用户流水归入 `user_cap`；不会把旧额度复制到每一个 Key。没有明确 Key 目标的余额标为 `legacy_unassigned`，管理员必须在切换 `enforce` 前通过已认证管理会话显式迁移，迁移未完成时保持 fail-closed。

启用 `enforce` 前逐项确认：备份 SQLite/WAL/SHM；迁移报告无未处理项；Key 已启用且 scope 正确；Key budget 版本为 ready；可选 User cap 已配置；未结束 reservation 和 `unknown` 任务已有 `event_group_id` 对账方案。Key、User cap 和上游账号 observation/lease 分开查看，不能把上游余额换算为用户额度。

worker 启动、重连或 lease 超时后先做 recovery。缺失一侧预算事件、状态不一致或上游结果未知都进入 `reconcile_required`，保留 held，不自动退款、补扣或换 Key 重放。Key 预算缺失、版本失效或迁移未完成时，`/v1/usage` 返回 409 `key_quota_not_configured`。

公网、LAN、本机都进入同一个 Core 鉴权和预算事务；Nginx/FRP 只负责 HTTPS、转发和长连接，不承担 API Key、scope 或额度授权。部署联调的临时 Rust target、日志和 npm 缓存统一使用 `D:\gpt`，避免占用 C 盘。

## 局域网

1. 网关监听 `0.0.0.0` 或指定内网网卡地址。
2. 创建并启用 API Key；LAN 监听禁止匿名访问。
3. Windows 防火墙仅放行可信内网网段的网关端口。
4. 客户端使用 `http://<内网IP>:<port>/v1`。

### 多路由器局域网（FRP + Nginx）

当不同路由器之间没有直接路由时，可在所有网段共同可达的中转机运行 `frps + Nginx`，
在 AI Work 主机运行 `frpc`，由 frpc 主动连接中转机再转发到本机 `127.0.0.1:7864`。
应用配置页提供 FRP 的保存、启动、停止和断线重启；部署模板见
`deploy/frp/`。这只桥接 HTTP API，不是完整二层局域网，SMB/mDNS 等广播协议不会被转发。

如果没有一台局域网中转机能被所有网段访问，则把同一套 `frps + Nginx` 放到腾讯云，
由办公室主机主动出站连接。不要把 7864 或 FRP 业务端口直接暴露到公网。

视频完成后默认写入 `AIWORK_VIDEO_DIR`，客户端通过
`GET /v1/videos/<task_id>/content` 获取，服务端不会把完整视频塞进 JSON 或模型消息。

`/health` 与 `/healthz` 只返回存活状态；账号、积分和错误明细应通过带 API Key 的
`/status` 查询。参考图/参考视频通过 `POST /v1/assets` 临时保存到 `AIWORK_ASSET_DIR`，
Seedance 任务提交时网关会使用当前 Trae Work 账号调用原生资源上传接口，再把返回的
`store_uri` 交给 Trae 签名。因此 `image_asset_ids` / `video_asset_ids` 不要求
`AIWORK_ASSET_PUBLIC_BASE_URL`，本机、局域网和公网部署都不需要让 Trae 云端回取调用方
电脑上的文件。该基址仍可选，用于客户端查看短时素材内容；默认链接保留 30 分钟，可用
`AIWORK_ASSET_TTL_SECS` 在 5 分钟至 2 小时内调整。账号切换时素材会按新账号重新上传，
不会复用旧账号的 `store_uri`。网关还按 API Key 限制素材上传次数/字节数和视频提交频率，
超限返回 429 与 `Retry-After`。

## 公网/服务器

建议使用办公室电脑主动建立 SSH 反向隧道，腾讯云只绑定回环端口，再由 Nginx/Caddy
提供 HTTPS。云安全组只开放 SSH 与 443；不要把 7864 裸露在公网。

```text
家中 CC/DSH → HTTPS → Nginx/Caddy → 服务器/隧道 → AI Work 网关
```

生产版应将视频目录放在独立卷或 MinIO/COS，并把任务索引从本地 JSON 迁移到 SQLite/
PostgreSQL。`video_tasks.json` 和 `data/videos` 是当前单机/小规模部署的兼容实现。

## Linux 无头版后续拆分

后续将把 `api_server`、账号池、刷新器、视频任务和存储抽到 `aiwork-core`，新增
`aiwork-server`（Axum/容器）作为数据面；Tauri、BitBrowser 和 Windows 桥保留为
`aiwork-desktop` 控制面。服务器只接收用户明确导出的加密账号资料，绝不搬运桌面
浏览器 profile。
# 星链维度分流系统（独立 Core）

独立程序 `starlink-dimension-router.exe` 默认监听 `127.0.0.1:7865`，管理页面为 `/admin`，对外 API 为 `/v1/*`。AI Work 仍是执行端，默认监听 `7864`；它只接受由 AI Work 管理界面生成、部署到 Core 的桥接管理员 Key。普通用户 API Key、积分、并发与任务归属全部由 Core 管理。

生产部署建议：Core 只通过 HTTPS 反向代理或可信内网暴露；公网反代到 7865，AI Work 的 7864 仅允许 Core 所在主机访问。先访问 `/healthz`，再在 `/admin` 中使用管理员账户登录，配置 AI Work Base URL 与桥接 Key，必须测试成功后才保存。桥接测试失败不会替换旧配置。

发布产物位于 `D:\gpt\starlink-dimension-router-release\release\starlink-dimension-router.exe`。迁移脚本默认只读检查源数据并输出哈希；只有明确提供 MigrationId 和 `-Apply` 才会复制 Core 数据库，绝不删除源 JSON、SQLite、素材或视频产物。

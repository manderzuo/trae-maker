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
必须提供真实 admin API Key；只读的 `core_status`、`core_migration_inspect` 是例外，具体
鉴权以当前实现为准。不能由客户端传入 actor 身份。Key 明文只在签发响应中显示一次，
Core 只保存 digest/prefix。

切换 `enforce` 前必须完成：迁移前备份、user、Key、scope、cost policy、grant 和 parity
report。旧 JSON 继续保留，不会被当作 Core grant 或真实上游余额。Phase 1 smoke 只使用
Mock executor；不得以真实 upstream、真实余额、真实计费或真实生成结果作为本阶段部署
验证证据。

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
`/status` 查询。参考图/参考视频通过 `POST /v1/assets` 临时保存到 `AIWORK_ASSET_DIR`。只有配置
`AIWORK_ASSET_PUBLIC_BASE_URL`（桌面端也可在网关设置页填写）时，`image_asset_ids` / `video_asset_ids` 才会转换为
带随机短时 token 的内容链接交给 Trae；未配置时素材不会离开网关，也不会自动改用
公网地址。该基址必须是 Trae 云端能够访问的 HTTPS 地址，不能填写 `127.0.0.1` 或
办公室内网地址。默认素材链接保留 30 分钟，可用 `AIWORK_ASSET_TTL_SECS` 在 5 分钟至 2 小时内调整；
仅隔离测试可设置 `AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 使用 HTTP。网关还按 API Key
限制素材上传次数/字节数和视频提交频率，超限返回 429 与 `Retry-After`。

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

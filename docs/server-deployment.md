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

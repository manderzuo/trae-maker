# Seedance MCP 接入

## 推荐入口：Agent Skill

跨客户端使用时，优先安装仓库内的 `skills/aiwork-seedance`。Skill 本体是
`SKILL.md`；`scripts/aiwork-seedance.ps1` 是它调用的本地执行器，MCP 只是给
不便直接运行 Skill 脚本的宿主提供的兼容入口。运行 `install.cmd` 会把 Skill
复制到标准的 `.agents/skills`，并在已存在的 Codex/Claude Code Skill 目录中
建立镜像；不会覆盖未知客户端的配置。

Skill 将长任务拆成 `seedance_submit`、`seedance_status`、`seedance_download`
三个逻辑阶段，避免宿主在一次工具调用中等待 900 秒。DSH/Qoder 等不扫描
`.agents/skills` 的客户端可直接调用同目录 PowerShell runner，或使用下方 MCP
配置；两种入口共享同一个 `/v1` HTTP 契约。

`src-python/seedance_mcp.py` 是一个零依赖的 stdio MCP 桥。它不读取 Trae、BitBrowser
或 JWT，只读取以下环境变量，然后调用 AI Work Assistant 网关：

```text
AIWORK_GATEWAY_BASE_URL=https://你的域名/v1
AIWORK_API_KEY=在 AI Work Assistant 中创建的 API Key
SEEDANCE_POLL_TIMEOUT=900
SEEDANCE_POLL_INTERVAL=3
# 仅当 Trae 上游能访问该地址时启用；否则不要填写
# 可选：也可在 AI Work 助手「API 服务 → 接口配置」中填写
AIWORK_ASSET_PUBLIC_BASE_URL=https://video.example.com/v1
# 仅隔离测试允许 HTTP；生产不要开启
# AIWORK_ALLOW_INSECURE_ASSET_BASE=true
```

## Claude Code

在 AI Work Assistant 的「API 管理 → 生态接入」点击「注册 Seedance MCP（Claude Code）」即可自动写入
CC Switch 的 `mcp_servers` 表。写入前会备份 CC Switch 数据库（包含 SQLite 的 WAL/SHM 文件）；完成后重启
CC Switch 和 Claude Code。该按钮只更新 `aiwork-seedance` 自有条目，不会覆盖其它 MCP 或 provider。

如果需要手动配置，使用下面的 stdio 服务：

在客户端 MCP 配置中注册一个 stdio 服务（Windows 示例）：

```json
{
  "mcpServers": {
    "aiwork-seedance": {
      "command": "python",
      "args": ["C:\\path\\to\\TraeWorkAssistant\\src-python\\seedance_mcp.py"],
      "env": {
        "AIWORK_GATEWAY_BASE_URL": "https://你的域名/v1",
        "AIWORK_API_KEY": "替换为 AI Work API Key"
      }
    }
  }
}
```

本机网关可使用 `http://127.0.0.1:7864/v1`；通过办公室 Nginx 的局域网客户端应使用
`http://中转机IP/v1`（例如 `http://192.168.0.17/v1`），不要直接暴露 7864 或 FRP 业务端口。
健康检查固定访问网关根路径 `/health`，不是 `/v1/health`。公网只使用 HTTPS 反向代理或 SSH 反向隧道。

## 工具参数

工具名为 `aiwork_health`、`aiwork_wait` 和 `seedance_generate`。先用 `aiwork_health` 做只读检查；
`seedance_generate` 必填 `prompt`，默认 5 秒、720p、16:9，可选 `duration`、`resolution`、`ratio`、
`image_urls`、`video_urls`、`image_paths`、`video_paths`、`image_asset_ids`、
`video_asset_ids`、`idempotency_key` 和 `download_path`。工具会创建任务并轮询
至 `completed`/`failed`，优先返回网关缓存的 `content_url`。指定 `download_path` 时，
视频会以临时文件 + 原子改名方式下载到调用方电脑。

### 本地参考图/参考视频

`image_paths` / `video_paths` 由 MCP 在**调用方电脑**读取，并只在这次工具调用中
逐个上传到网关的 `POST /v1/assets`；网关会检查文件魔数、类型、大小（单个最多
32 MiB）并按 API Key 隔离。不会扫描目录，也不会把 `C:\` 路径直接发送给 Trae。

上传后视频请求携带 `image_asset_ids` / `video_asset_ids`。要让 Trae 上游读取这些
素材，网关主机必须显式设置 `AIWORK_ASSET_PUBLIC_BASE_URL`（或在网关设置页填写），指向 Trae 能访问的
HTTPS 网关前缀，例如 `https://example.com/v1`。生产默认拒绝 HTTP；仅在隔离测试中显式设置
`AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 才允许。网关生成带随机短时 token 的
`/v1/assets/<id>/content` 地址，默认 30 分钟后失效（可用 `AIWORK_ASSET_TTL_SECS` 调整，范围 5 分钟至 2 小时）。未设置该变量时，
素材仍可安全存储，但生成请求会明确返回“未配置公网素材基址”，不会猜测性上传。

本机/局域网地址（`127.0.0.1`、`192.168.x.x`）通常不能被 Trae 云端回取；这两种
部署要么继续使用 `image_urls` 提供已公开可达的地址，要么待确认 Trae 原生参考图
上传协议后启用原生上传适配器。公网/腾讯云反向代理场景可把该地址设置为 HTTPS
域名，并只在用户明确开启时暴露素材端点。

例如 DSH/Claude Code 的工具参数可以直接写：

```json
{
  "prompt": "沿用参考图中的橘猫，镜头缓慢推近，5 秒，16:9",
  "image_paths": ["C:\\Users\\me\\Pictures\\cat.png"],
  "duration": 5,
  "resolution": "720p",
  "ratio": "16:9"
}
```

若客户端只能传 URL，也可以继续使用 `image_urls` / `video_urls`；网关不会替换或
下载这些外部 URL。

## DSH

DSH 的自定义 Provider 继续配置文字模型的 `/v1` 地址；视频不能仅通过选择模型名实现，
需要把本桥注册为 DSH 的自定义工具/插件，或直接调用同一个 HTTP 视频端点。

## 存储边界

- 视频首先由网关保存到 `data/videos`，可用 `AIWORK_VIDEO_DIR` 指向独立磁盘。
- 任务索引保存到 `data/video_tasks.json`，网关重启后可继续查询。
- 参考素材保存到 `data/assets`（可用 `AIWORK_ASSET_DIR` 指定目录），索引为
  `data/assets.json`；默认保留 30 分钟。公网内容链接使用随机 token，不携带 API Key，响应不缓存且带
  `X-Content-Type-Options: nosniff` 与 `Referrer-Policy: no-referrer`。
- 客户端只拿到 API Key 允许访问的任务和内容地址；桥不会把 JWT、Cookie 或 API Key
  写入日志。

## 网关安全与限流

- `/health` 与 `/healthz` 只返回存活状态（`status`、`running`），不会公开账号数、积分或当前 UID；详细诊断使用受 API Key 保护的 `/status`。
- 本地路径上传只接受 PNG/JPEG/GIF/WebP/MP4/WebM 魔数，单文件上限 32 MiB；MCP 会在读取完整文件前拒绝未知格式。
- 默认启用按 Key 的保护：全局并发 32；每个 Key 每分钟最多 30 次素材上传、每小时 256 MiB；每分钟最多 3 次视频提交。可通过
  `AIWORK_MAX_INFLIGHT`、`AIWORK_ASSET_UPLOADS_PER_MINUTE`、`AIWORK_ASSET_BYTES_PER_HOUR`、
  `AIWORK_VIDEO_SUBMISSIONS_PER_MINUTE` 调整，修改后重启网关生效。超限返回 HTTP 429 与 `Retry-After`。
- 客户端访问地址可以是局域网 HTTP；但给 Trae 云端回取素材的 `AIWORK_ASSET_PUBLIC_BASE_URL` 仍应使用 HTTPS。

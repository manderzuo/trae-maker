# Seedance MCP 接入

Seedance 现在也支持直接 HTTP API：MCP/Skill 是可选适配层，不是视频生产的必要条件。
能读取 `/v1/models` 能力元数据的客户端可以使用同一 Base URL 和 API Key 选择
`seedance`，再按模型项的 `endpoint` 进入异步视频流程；普通文字客户端仍调用文字端点。

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
至 `completed`/`failed`，完成后自动以临时文件 + 原子改名方式下载到调用方电脑的
`Downloads` 文件夹。指定 `download_path` 可覆盖默认位置；对用户只返回本地文件路径，
不返回任务查询地址。

### 本地参考图/参考视频

`image_paths` / `video_paths` 由 MCP 在**调用方电脑**读取，并只在这次工具调用中
逐个上传到网关的 `POST /v1/assets`；网关会检查文件魔数、类型、大小（单个最多
32 MiB）并按 API Key 隔离。不会扫描目录，也不会把 `C:\` 路径直接发送给 Trae。

上传后视频请求携带 `image_asset_ids` / `video_asset_ids`。网关会按当前
Trae Work 账号执行原生资源上传：请求上传地址、PUT 原始字节、提交上传结果，
再把返回的 `store_uri` 放入 Seedance 的 `image_urls` / `video_urls`。因此本机、
局域网和公网部署都不要求 Trae 云端能访问调用方电脑，也不要求配置公网素材基址。
`AIWORK_ASSET_PUBLIC_BASE_URL` 仍可用于客户端查看临时素材，但不参与 Seedance
输入图签名。账号切换时会用新账号重新上传，不能复用旧账号的 `store_uri`。

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

`image_urls` / `video_urls` 只接受 Trae 原生资源 URI（例如 `tos-cn-...`）。任意
公网 `https://...` 地址不会直接转发给 Seedance；请先通过 `/v1/assets` 上传，
再使用返回的资产 ID。这样可以避免上游把公网 URL 当作原生 URI 签名而报
`invalid uri`，也避免网关主动下载不受信任的外部地址。

## DSH

DSH 的自定义 Provider 继续配置同一个 `/v1` 地址和 API Key。若 DSH 版本支持
`GET /v1/models` 返回的 `capabilities`/`endpoint` 元数据，选择 `seedance` 后可直接进入
视频生产流程；若 DSH 只支持固定的 OpenAI 文字协议，则使用本桥、Skill，或直接调用下面的
视频端点。

```text
POST {BASE_URL}/videos/generations
Authorization: Bearer <AIWORK_API_KEY>
Content-Type: application/json

{"model":"seedance","prompt":"一只猫在窗边看雨，电影感镜头","duration":4,"resolution":"720p","ratio":"16:9"}
```

底层接口返回任务 ID 后由桥在内部轮询 `GET {BASE_URL}/videos/{task_id}`，完成后读取
`GET {BASE_URL}/videos/{task_id}/content`。这里的 `{BASE_URL}` 已包含 `/v1`，
局域网和公网只需替换 Base URL，不需要改 Key 或另配 MCP；正常使用不需要向用户展示
任务 ID 或查询地址。

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
- 客户端访问地址可以是局域网 HTTP；`AIWORK_ASSET_PUBLIC_BASE_URL` 若启用，仅用于
  客户端查看短时素材内容，生产环境仍应使用 HTTPS。
# Seedance 与独立 Core

Core 是统一的公网入口和额度中转层。需要参考图或参考视频时，先在 Core 管理界面创建普通 API Key，勾选“视频生成”和“素材上传”，并为该 Key 分配积分；然后把下面的地址和 Key 配置到客户端：

```text
公网：   https://api.gemstory.cn/v1
局域网： http://中转机IP/v1
本机：   http://127.0.0.1:7865/v1
```

同一个 Core Base URL 和普通 API Key 同时支持文字与视频。文字请求调用 `/chat/completions`；视频请求调用 `/videos/generations`，或在兼容客户端中选择 `model=seedance`。Core 会按请求类型记账，再使用内部 AI Work 桥接配置转发到 AI Work。客户端不需要填写 AI Work 桥接 Key，也不需要把 Core 管理员登录信息放进客户端。

公网素材地址保存在 Core 数据目录的 `router.json` 中（当前为 `https://api.gemstory.cn`），不是只保存在当前终端环境变量里。重启程序或电脑后，启动脚本只需继续使用同一个 `D:\gpt\starlink-dimension-router-data` 数据目录即可恢复该配置。

权限建议：

- 纯文字：`chat:invoke`；
- 纯文字生成视频：`videos:submit`；
- 带参考图/参考视频：同时勾选 `videos:submit` 和 `assets:write`；
- 如果客户端只是调用已上传的资产 ID，仍建议保留 `assets:write`，便于 MCP 在本机素材变化后重新上传。

MCP/Skill 不是额度管理入口，而是不能直接调用视频端点的 Agent 的兼容适配层。MCP 会先把本地参考素材上传到 `POST /v1/assets`，再把返回的资产 ID 交给视频接口；不会把本地路径直接发送给上游。Core 会在内部完成素材到 AI Work 原生资源的转换。Seedance Chat 兼容入口不支持 `stream=true`，请关闭流式输出；直接视频端点则由 MCP 内部轮询并自动下载到调用方电脑的 `Downloads` 文件夹，不向最终用户展示查询地址。

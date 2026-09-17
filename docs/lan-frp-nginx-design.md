# AI Work 多路由器网络接入设计方案

## 1. 目标与边界

目标是在不把 AI Work Assistant 运行环境写死的前提下，让不同路由器、不同网段下的 DSH/CC/其他 Harness 访问同一个 AI Work API，并支持 Seedance 参考图/参考视频上传。

本方案打通的是 HTTP API，不是完整的二层局域网桥接：

- 支持 `/health`、文本 API、Seedance 视频 API、素材上传和结果下载。
- 不承诺 SMB、局域网广播、mDNS、打印机发现等二层/广播协议。
- 路由器数量不是主要限制，关键是各网段能否到达中转节点，或 AI Work 主机能否主动连到中转节点。

## 2. 三种运行模式

### 2.1 单机模式

```text
DSH/CC ── 127.0.0.1:7864 ── AI Work Assistant
```

适用于所有组件在同一台电脑上的情况。不启用 FRP/Nginx，网关监听 `127.0.0.1`，风险最低。

### 2.2 多路由器纯局域网模式（本次重点）

```text
路由器 A 网段 ─┐
路由器 B 网段 ─┼── 可被各网段访问的局域网中转机
路由器 C 网段 ─┘       ├─ frps
                         └─ Nginx（可选，但推荐）
                              ↑
                    frpc（AI Work 主机）
                              ↑
                    127.0.0.1:7864
```

部署原则：

1. `frps + Nginx` 放在所有相关网段都能访问的上级网络、核心交换机网段或稳定服务器上。
2. `frpc` 只安装在运行 AI Work Assistant 的主机上。
3. AI Work 网关继续监听 `127.0.0.1:7864`，不直接暴露到各个网段。
4. 其他电脑统一访问中转机的 Nginx 地址，例如 `http://aiwork-relay.lan` 或 `https://aiwork-relay.lan`。

如果中转机只能被部分路由器访问，FRP 无法凭空修复物理不可达的网络；此时应把中转机上移到更高一级网络，或改用腾讯云模式。

### 2.3 腾讯云公网中转模式

```text
家中/外部客户端 ─HTTPS─> 腾讯云 Nginx ─> frps
                                      ↑
                         frpc（办公室 AI Work 主机主动连接）
                                      ↑
                               AI Work:7864
```

适用于各办公网段之间完全不可路由，或需要在家中/外部网络访问的情况。公网只开放 Nginx 的 443 和 SSH 管理端口，不直接暴露 AI Work 7864。

## 3. 请求链路

### 3.1 普通 API

```text
客户端 → Nginx → frps/frpc → AI Work 7864 → 账号池/Trae Work
```

客户端只需要配置：

```text
Base URL: http(s)://中转地址/v1
API Key : AI Work 网关中创建的 Key
```

### 3.2 参考图/参考视频

```text
本地文件/拖拽附件
      ↓ POST /v1/assets
Nginx → FRP → AI Work 临时素材目录
      ↓ 返回 asset_id
POST /v1/videos/generations（携带 asset_id）
      ↓
AI Work 生成带短时 token 的 content_url
      ↓
Seedance 上游读取参考素材
```

设计约束：

- 单个素材默认不超过 32 MiB，服务端按文件头校验 PNG/JPEG/GIF/WebP/MP4/WebM。
- 素材按 API Key 隔离，默认 30 分钟过期（可配置为 5 分钟至 2 小时），写入采用临时文件后原子替换。
- 不扫描客户端目录，不上传未明确选择的文件。
- LAN 模式只有当 Seedance 上游能够访问素材 URL 时，参考图才可直接用于云端生成。
- 如果 Trae 云端无法访问局域网地址，必须使用腾讯云 HTTPS 模式，或把素材放到可访问的对象存储后再提交。

## 4. 组件与端口规划

| 组件 | 所在位置 | 建议监听 | 用途 |
|---|---|---:|---|
| AI Work 网关 | AI Work 主机 | `127.0.0.1:7864` | 账号池、文本 API、Seedance API、素材服务 |
| frpc | AI Work 主机 | 出站连接 | 将本机 7864 转发到中转机 |
| frps | 局域网中转机或腾讯云 | `:7000` | FRP 控制连接 |
| FRP 业务端口 | 中转机 | 例如 `:17864` | Nginx 的上游入口，防火墙限制为本机/可信网段 |
| Nginx | 中转机 | LAN `:80`/`:443` | 统一入口、TLS、请求体限制、长请求超时 |

端口不应写死在代码中，放入配置页或环境变量：

```text
AIWORK_BIND=127.0.0.1
AIWORK_PORT=7864
AIWORK_PUBLIC_BASE_URL=https://api.example.com
AIWORK_ASSET_PUBLIC_BASE_URL=https://api.example.com/v1
AIWORK_CORS_ORIGINS=https://console.example.com
```

LAN 环境可将 `PUBLIC_BASE_URL` 设置为中转机的 HTTPS 地址；单机模式保持为空，避免把本地素材错误地暴露出去。

## 5. FRP 与 Nginx 配置原则

### 5.1 FRP

- 使用 token 或更强认证，禁止匿名 FRP。
- 启用 TLS，控制端口仅允许可信网段或固定主机访问。
- 每个 AI Work 主机使用独立 proxy 名称和独立 API Key。
- 多个 AI Work 主机时为每个实例分配独立域名/路径/远程端口。
- 不把配置文件中的 token、API Key、私钥提交到仓库或聊天记录。
- frpc 断线自动重连，并由 Windows 服务/任务计划或应用托盘控制启动。

### 5.2 Nginx

推荐规则：

- 只对外开放 80/443；FRP 业务端口由防火墙限制，不直接给客户端使用。
- `client_max_body_size` 至少 46 MiB，以容纳 32 MiB 素材和请求开销。
- 视频轮询、长任务和 SSE 关闭代理缓冲，并设置不低于 900 秒的读取超时。
- 转发 `Host`、`X-Forwarded-For`、`X-Forwarded-Proto`。
- 只允许 HTTPS 生产访问；LAN 客户端入口测试可以先用 HTTP，但 API Key 仍必须启用。给 Trae 云端回取素材的
  `AIWORK_ASSET_PUBLIC_BASE_URL` 默认强制 HTTPS，只有隔离测试显式设置 `AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 才能使用 HTTP。
- 可按域名、网段或额外 Basic/mTLS 做第二层限制，API Key 作为应用层认证。
- `/health` 与 `/healthz` 只返回存活字段；不要把账号池/积分诊断放到免鉴权路径。

## 6. 网络可行性判断

当前多路由器场景满足下面条件时可行：

1. AI Work 主机能主动连接中转机的 FRP 控制端口。
2. 各客户端能访问中转机的 Nginx 地址。
3. 路由器没有开启客户端隔离，或已放行到中转机的端口。
4. Windows 防火墙允许中转机 Nginx 端口，且 AI Work 主机本地只允许 frpc 访问 7864。

如果第 1 条或第 2 条不成立，纯局域网方案不能成立；应将 `frps + Nginx` 移到上级网络，或使用腾讯云作为共同可达的中转点。

## 7. 应用侧改造建议

### 第一阶段：可用性

- API 服务页增加“监听地址、端口、公开基址、素材公开基址、FRP 状态”展示。
- 启动 API 网关后显示实际监听地址和健康状态。
- 支持“启动网关 + 启动 frpc”的顺序控制，网关失败时不启动隧道。
- FRP 断线时在 UI、日志和 `/health` 中明确显示，不伪装成账号池空。

### 第二阶段：安全与运维

- 配置文件与密钥分离，密钥保存在系统凭据或应用私有目录。
- 增加连接保活、重连次数、最后成功时间和最近错误。
- 停止网关时同步停止 frpc，避免留下悬空公网入口。
- 为每台客户端记录 API Key 使用量，但不记录素材正文、JWT 或完整 Authorization。

### 第三阶段：多实例与云部署

- 将 `relay` 配置抽象为 `disabled / lan / cloud` 三种模式。
- 每个实例使用独立 `instance_id`、proxy 名称和公开域名。
- 未来把网关、任务索引、素材目录和视频目录拆成可迁移的服务端组件；桌面端只保留登录态和账号控制能力。

## 8. 分阶段实施与验收

### 阶段 A：不消耗积分

1. 新版本启动，手动启动 API 网关。
2. 中转机安装并启动 frps，AI Work 主机启动 frpc。
3. 从每个路由器网段访问 `/health`，确认返回 `status=ok`。
4. 用错误 API Key 验证 401，用正确 API Key 验证正常访问。
5. 上传一张小 PNG 到 `/v1/assets`，确认返回 `asset_id`，并验证跨 Key 不能读取。
6. 下载已上传素材和删除/过期清理，确认权限与 TTL 正常。

### 阶段 B：最小成本业务测试

1. 提交 Seedance 参数校验请求，不发起真实生成。
2. 用用户明确授权的最低成本提示词做一次图生视频。
3. 验证长轮询、失败透传、结果下载和视频文件落盘。
4. 关闭 frpc、Nginx、AI Work，分别验证健康状态和错误提示。

### 阶段 C：公网桥接

1. 腾讯云只开放 SSH/443，frps 控制端口限制来源。
2. Nginx 配置正式域名和证书。
3. 外部电脑仅配置 HTTPS Base URL 和 API Key。
4. 验证参考素材 URL 能被 Seedance 上游访问后，再启用正式图生视频。

## 9. 推荐落地顺序

1. 先确认多路由器之间的“中转机”位置和固定 IP。
2. 先做 LAN `frps + frpc`，Nginx 可先用 HTTP 验证链路。
3. 链路通过后再加 Nginx HTTPS、API Key、素材上传测试。
4. 最后再复制同一套配置到腾讯云公网模式。
5. 不建议一开始同时改网络、账号池和视频生成逻辑；每一层先用健康检查和小文件测试验收。

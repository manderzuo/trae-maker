# Image Relay Hardening Design

**Date:** 2026-09-17

## Goal

让本机、局域网和公网客户端携带首帧图片/参考视频调用 Seedance 时，保持现有 API 兼容，同时收紧公网信息泄露、临时素材链接、URL 输入、资源消耗和本地文件读取边界。

## Scope

本阶段只改四个边界：Rust API 网关、素材临时仓、Python MCP 桥和 FRP/Nginx 部署示例。不改变 Trae Work 原生 Seedance 请求格式，不发起真实视频生成，不修改账号池或积分调度。

## Design

1. `/health` 只返回存活信息；账号、积分、UID 和错误明细继续由鉴权保护的 `/status` 提供。`/healthz` 作为不带敏感数据的探活端点，加入反代示例。
2. 临时素材 URL 继续使用随机 token 供 Trae 云端回取，但默认 TTL 调整为 30 分钟，可用 `AIWORK_ASSET_TTL_SECS` 在 5 分钟至 2 小时内显式调整。素材响应使用 `Cache-Control: no-store`、`Referrer-Policy: no-referrer`、`X-Content-Type-Options: nosniff`。
3. 公开素材基址默认必须为 HTTPS；仅设置 `AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 时允许 HTTP，用于明确的本机/LAN 测试。公网部署不启用该开关。
4. 新增进程内、按 API Key 隔离的资源限制器：全局并发上限、每 Key 每分钟素材上传次数、每 Key 每小时素材字节数、每 Key 每分钟视频提交次数。限制命中返回 429，并在响应中给出 `Retry-After`。
5. `image_urls`/`video_urls` 只允许 `https://`，HTTP 仅在显式不安全开关打开时允许；拒绝 `file:`、`data:`、`ftp:`、回环/私网/保留 IP 字面量和缺少主机的 URL。网关不下载外部 URL，但不把明显危险地址转交给 Trae。
6. MCP 读取 `image_paths`/`video_paths` 前先读取少量文件头并校验支持的 PNG/JPEG/GIF/WebP/MP4/WebM 魔数；未知文件在本机被拒绝，不会先读取完整内容再上传。

## Compatibility

- `POST /v1/assets`、`POST /v1/videos/generations`、任务查询和已有 API Key 头保持不变。
- 本机/局域网客户端的 `AIWORK_GATEWAY_BASE_URL` 仍可使用 HTTP；只有交给 Trae 云端回取的 `AIWORK_ASSET_PUBLIC_BASE_URL` 默认要求 HTTPS。
- 既有 `daily_limit` 仍有效；新限制器是额外的突发保护，不改变按日请求计数。

## Testing

- Rust 单测覆盖健康响应脱敏、HTTPS/HTTP 基址策略、URL 校验、限流窗口和素材响应安全头。
- Python 单测覆盖本地非法文件在读取完整内容前被拒绝。
- 运行 Rust 全量测试、Python 单元测试、Skill smoke/integration、前端构建与 UI 按钮扫描。
- 不调用真实 Seedance 生成；公网只做 `/health`、`/healthz`、无 Key 鉴权和本地测试网关检查。

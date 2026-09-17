# FRP + Nginx 部署模板

## 适用拓扑

```text
AI Work 主机（127.0.0.1:7864）
        │ frpc 主动出站
        ▼
共同可达的局域网中转机或腾讯云（frps:7000）
        │
        └── Nginx（80/443） → 127.0.0.1:17864
```

局域网模式下，中转机必须能被相关路由器网段访问；公网模式下，中转机是腾讯云。FRP 转发的是 HTTP API，不是完整二层局域网。

## 部署顺序

1. 在中转机安装与 frpc 同版本的 frps，复制 `frps.toml.example`，生成高强度 token。
2. 在中转机启动 frps，并在防火墙放行控制端口 `7000`；业务端口 `17864` 只允许本机 Nginx 访问。
3. 在 AI Work 主机安装 frpc。可在“接口配置 → FRP 局域网 / 公网接入”填写地址、端口、token 和 frpc 路径。
4. 在 AI Work 主机先启动 API 网关，再启动 frpc。网关保持监听 `127.0.0.1:7864`。
5. 在中转机安装 Nginx，复制 `nginx.aiwork.conf.example`，把上游保持为 `127.0.0.1:17864`。
6. 从每个路由器网段请求 `http(s)://中转机/health` 或 `/healthz`，确认只返回 `status=ok` 后再配置 DSH/CC 的 `/v1` Base URL。

## 安全要求

- frps/frpc token 只保存在本机私有配置，不提交 Git，不发到聊天中。
- 公网环境只开放 SSH 和 Nginx 443；不要开放 AI Work 7864 或 FRP 业务端口给公网。
- LAN 也必须启用 AI Work API Key；FRP token 只保护隧道，不能替代应用鉴权。
- 参考图/视频通过 `/v1/assets` 上传；要让 Trae 云端回取，`AIWORK_ASSET_PUBLIC_BASE_URL` 必须是 Trae 可访问的 HTTPS 地址。
- `/health`、`/healthz` 不包含账号数和积分；详细状态请用带 API Key 的 `/status`。
- 生产素材基址默认拒绝 HTTP；只有隔离测试显式设置 `AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 才可例外。

## 无积分验收

```text
GET /health
GET /v1/models（携带 API Key）
POST /v1/assets（上传一张小 PNG）
GET /v1/assets/<id>/content（同一个 API Key）
```

先完成上述链路，再进行用户明确授权的最低成本 Seedance 测试。

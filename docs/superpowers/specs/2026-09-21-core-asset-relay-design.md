# Core 公网素材中转与视觉请求设计

日期：2026-09-21  
状态：待用户审阅

## 1. 目标

让 Core 公网入口成为素材能力的统一入口，使同一个公网 Base URL 和 Core 用户 API Key 可以支持：

1. MCP 上传本地图片、参考视频并生成 Seedance 视频；
2. 通过 `image_asset_ids` / `video_asset_ids` 复用已上传素材；
3. 支持图片识别的文字模型继续使用 OpenAI 兼容的 `image_url` 图片内容；
4. Core 统一执行 API Key 鉴权、作用域、并发、素材大小/频率、用户隔离和额度记录；
5. AI Work 继续负责上游账号选择、原生素材上传、视频任务和文字模型调用。

本改造不把 Core 用户 API Key 传给 AI Work，也不把素材正文、Key 明文或完整提示词写入请求日志。

## 2. 当前缺口与边界

AI Work 已有 `/v1/assets`、素材 TTL、内容令牌和 Core 素材存储实现，但公网星链维度分流系统目前只注册了模型、聊天和视频路由，没有注册 `/v1/assets`。

Core 到 AI Work 的桥接会用专用桥接 Key 覆盖用户 Authorization，这是既有安全边界，不能通过“原样转发用户 Key”来修复素材问题。公网 Core 与 AI Work 可能使用不同数据目录，因此不能假设两者共享素材文件或 SQLite 文件。

本次范围包括 Core 公网素材接口、桥接侧素材物化、视频请求中的素材解析、必要的文字视觉输入适配和管理界面作用域显示；不包括真实上游自动化测试、素材永久存储、匿名上传、跨用户素材共享或把公网 URL 直接交给上游。

## 3. 公开 API 契约

### 3.1 上传素材

`POST /v1/assets`

请求使用当前 MCP 已采用的 JSON 形式：

```json
{
  "filename": "reference.png",
  "mime_type": "image/png",
  "data_base64": "iVBORw0KGgo..."
}
```

也接受带 MIME 头的完整 data URL。请求必须带 Core 用户 API Key，并且 Key 具有 `assets:write` 作用域。

成功响应保持 MCP 现有字段兼容：

```json
{
  "object": "asset",
  "id": "asset-...",
  "filename": "reference.png",
  "mime_type": "image/png",
  "bytes": 12345,
  "sha256": "...",
  "created_at": 1720000000,
  "expires_at": 1720001800,
  "content_url": "https://api.example/v1/assets/asset-.../content?token=..."
}
```

素材 ID、文件内容、摘要和过期时间绑定到 Core 用户；同一用户之外的 Key 不得读取或引用该素材。

支持的格式为 PNG、JPEG、GIF、WebP、MP4、WebM。单文件限制沿用现有 AI Work 素材上限；Base64 请求体额外保留编码膨胀空间。Core 在接收前执行请求体上限检查，在写入前执行文件魔数、MIME、大小和文件名校验。

### 3.2 内容读取

`GET /v1/assets/{asset_id}/content?token=...`

内容地址使用短时随机令牌，不要求调用方携带 API Key，便于上游或模型读取；令牌只对应一个素材，过期、错误令牌、路径异常或摘要不匹配统一返回 not-found 语义。上传、创建、引用和管理接口仍必须使用 API Key。

### 3.3 视频请求

`POST /v1/videos/generations` 继续支持：

```json
{
  "model": "seedance",
  "prompt": "让画面动起来",
  "image_asset_ids": ["asset-..."],
  "video_asset_ids": ["asset-..."],
  "duration": 5,
  "resolution": "720p",
  "ratio": "16:9"
}
```

Core 先验证素材属于当前用户、处于 active 状态且格式与字段匹配，再为本次桥接请求生成内部素材副本/引用。发送给 AI Work 的请求不包含用户的原始素材 ID，避免把 Core 内部 ID 当成 AI Work 素材 ID。

MCP 的现有流程不变：上传素材 → 取得 Core 素材 ID → 提交视频 → 轮询 → 自动下载到本机 Downloads。

### 3.4 图片识别文字请求

普通文字模型继续使用 OpenAI 兼容的消息内容：

```json
{
  "model": "支持视觉的文字模型",
  "messages": [
    {
      "role": "user",
      "content": [
        {"type": "text", "text": "请识别图片内容"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
      ]
    }
  ],
  "stream": false
}
```

该标准格式由 Core 原样保留并转发，Core 不把图片改造成文字，也不调用默认文字模型替用户补全视觉请求。若后续客户端使用 Core `image_asset_ids` 扩展，Core 会在转发前按当前用户权限读取素材，并转换为受大小约束的标准 `image_url` 数据内容；该扩展不影响普通 OpenAI 客户端。

## 4. 内部桥接设计

### 4.1 Core 侧素材存储

星链维度分流系统新增专用素材模块，使用自己的运行数据目录保存：

- `data/assets/{asset_id}.{ext}`：原子写入的文件；
- Core SQLite `assets` 表：用户归属、大小、摘要、存储引用、令牌摘要、创建/过期时间和状态。

Core Store 已有素材记录和用户归属查询能力，路由层复用该能力，不新建第二套用户额度账本。过期清理只清理明确过期的素材文件和记录，不删除用户数据或未过期素材。

### 4.2 桥接素材物化

当 Core 收到带素材 ID 的视频请求时：

1. Core 根据当前 `Principal` 校验全部素材归属和状态；
2. Core 读取并重新校验文件大小和 SHA-256；
3. Core 使用专用桥接 Key 调用 AI Work 的内部素材上传能力，得到 AI Work 侧素材 ID；
4. Core 仅在发送给 AI Work 的视频体内替换 `image_asset_ids` / `video_asset_ids`，不改变用户看到的 Core 素材 ID；
5. AI Work 按现有原生素材上传路径把桥接侧素材上传给上游；
6. AI Work 返回的任务和内容地址继续由 Core 进行用户任务绑定与转发。

桥接上传失败时，Core 不提交视频任务，并释放本次尚未提交的额度预留；若桥接请求超时无法确定结果，沿用现有 fail-closed/待对账规则，不自动重复真实视频提交。

桥接侧临时素材由 AI Work 现有 TTL 清理机制回收。Core 侧素材 TTL 与桥接请求生命周期保持一致，避免永久复制。

### 4.3 用户 Key 与内部 Key

- Core 用户 Key：只在公网 Core 入口验证、计额度和计并发；
- AI Work 桥接 Key：只在 Core 到 AI Work 的内部请求中使用；
- Core 管理员凭据：不进入 MCP、视频请求或素材请求；
- Core 不保存用户 Key 明文，桥接层不记录任何 Authorization 内容。

## 5. 权限、额度和限流

新增/启用 `assets:write` 作用域。Core 管理界面的作用域用中文展示：

- 文字处理；
- 视频生成；
- 素材上传。

素材上传不额外消耗视频或文字积分；视频提交和文字请求仍按各自资源策略计费。素材上传受以下 Key 级限制：

- 同时进行中的上传/转发请求不超过 Key 并发上限；
- 每分钟上传次数限制；
- 每小时上传字节限制；
- 单文件大小限制；
- 用户只能引用自己的素材。

超限统一返回明确的 401/403/413/429 错误，并在 429 中带 `Retry-After`。额度不足时不写入部分 Core 请求账本，不重复扣费。

## 6. 错误与安全处理

- 缺少 `assets:write`：403 `insufficient_scope`；
- 未认证：401 `api_key_required`；
- Base64、魔数、MIME 或大小错误：400 `invalid_asset`；
- 超过请求体上限：413 `request_too_large`；
- 频率、容量或并发超限：429；
- 不属于当前用户、已过期或令牌错误：404 `asset_not_found`；
- AI Work 桥接不可用：502，并记录可对账的请求 ID，不伪造成功任务；
- Core 资产存在但 AI Work 素材物化状态不确定：进入已有的待对账语义，不自动重放真实生成请求。

日志只记录请求 ID、Key 前缀/内部摘要、素材 ID 的不可逆摘要、状态和大小统计，不记录图片内容、Base64、视频正文、完整 prompt、Cookie、JWT 或 Key 明文。

## 7. 管理界面变化

Core 管理界面的普通 API Key 创建和编辑区域新增“素材上传”作用域勾选。Key 列表继续显示状态、并发、额度和使用情况，但不显示完整 Key 或素材正文。

首页的运行与额度概览不增加素材正文或用户敏感信息；素材接口状态可作为设置/诊断项展示，包括上传上限、TTL 和桥接状态。

## 8. 测试与验收

所有测试、Cargo/npm 缓存和构建目标优先使用 `D:\gpt`。

### 8.1 Core 路由测试

- 正确作用域可以上传图片和视频素材；
- 缺少作用域、错误 Key、禁用 Key 会被拒绝；
- 非法 Base64、伪造 MIME、未知文件魔数、路径穿越文件名和超限文件被拒绝；
- 同一用户可以读取自己的内容令牌，不同用户、错误令牌和过期令牌不可读取；
- `image_asset_ids` / `video_asset_ids` 只允许引用当前用户资产；
- bridge 记录只使用专用 Key，不透传用户 Authorization；
- 资产桥接失败时不产生成功视频任务、不留下未结算扣费。

### 8.2 MCP 回归测试

使用 fake HTTP bridge，不访问真实上游，验证：

1. `AIWORK_GATEWAY_BASE_URL=https://api.gemstory.cn/v1`；
2. MCP 上传本地 PNG/MP4 到 Core `/v1/assets`；
3. 视频请求只携带 Core 返回的资产 ID；
4. Core 返回任务后 MCP 查询并保存到 Downloads 目录；
5. 无素材的纯文字视频流程保持兼容。

### 8.3 生产验收

只做本地 fixture/fake adapter 和健康检查，不自动调用真实视频上游。正式上线后由用户手动使用一个有 `videos:submit`、`assets:write` 和足够额度的测试 Key 验证：上传一张小 PNG、提交图生视频、确认生成文件进入 Downloads，再验证另一枚 Key 无法读取该素材。

## 9. 非目标

- 不允许匿名素材上传；
- 不把公网任意 URL 直接转发给上游作为可靠素材输入；
- 不用 Token 数量替代积分额度；
- 不让 MCP 使用 Core 管理员 Key；
- 不改变 Seedance `stream=false` 的异步视频协议；
- 不在自动化测试中发送真实上游请求；
- 不删除已有用户数据或生产素材。

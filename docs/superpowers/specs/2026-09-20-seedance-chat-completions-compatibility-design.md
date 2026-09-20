# Chat Completions 自动转 Seedance 设计规格

日期：2026-09-20  
状态：待确认  
范围：将 `POST /v1/chat/completions` 中的 `model=seedance` 请求直接适配到现有 Seedance 视频任务链路。

## 1. 目标

实现一个统一入口：

- `model` 规范化后等于 `seedance` 时，Chat Completions 请求进入视频生产流程；
- 其他模型保持现有文字路由、调度、配额和响应行为不变；
- 不要求额外调用默认文字模型。Seedance 适配器直接使用用户提交的文字提示词；
- 图片消息可以作为 Seedance 参考图进入现有素材上传链路；
- Legacy 与 Core enforce 两种运行模式都沿用现有视频能力、真实积分、并发限制和幂等控制；
- 不向真实上游发送测试请求，所有自动化验收只使用 D 盘 Mock。

这份规格取代早期“Chat Completions 不做多态视频入口”的设计边界；独立的 `/v1/videos/generations` 仍然保留，现有客户端不受影响。

## 2. 非目标

本次不做以下内容：

- 不把普通文字模型调用自动改成视频；只有明确选择 `seedance` 才触发视频分流；
- 不默认调用设置中的文字模型来改写、扩写或审核提示词；
- 不让网关读取外部电脑的本地文件路径；
- 不允许网关为处理参考图而无条件抓取任意公网 URL；
- 不改变既有 MCP 高层工具“完成后自动下载到调用方 Downloads、不给用户查询地址”的行为；
- 不移除或替换原有 `/v1/videos/*` 查询、取消和内容下载接口。

## 3. 入口判定

在 `chat_completions` 完成 JSON 解析、请求大小校验和基础字段读取后，先做模型规范化判定：

```text
trim + ASCII case-insensitive compare(model, "seedance")
```

若命中：

1. 不进入 `dispatch::resolve_target`、文字池、WorkBuddy 或普通 Trae 文字执行器；
2. 不执行文字 token 预留；
3. 进入新的 `seedance_chat_adapter`，将请求投影为视频请求；
4. 视频能力和视频积分由现有视频链路处理。

若未命中：保持现有 `chat_completions` 流程完全不变。

`/v1/models` 中的 `seedance` 元数据继续声明为视频模型；为了兼容只会调用 Chat Completions 的客户端，额外补充兼容字段，明确表示该模型可接受 Chat Completions 投影，但真实结果是异步视频任务。

## 4. Chat 请求到视频请求的投影

### 4.1 必填输入

- `messages` 必须是非空数组；
- 至少包含一条 `role=user` 消息；
- 从最后一条 user 消息提取视频提示词；
- 提示词去除首尾空白后不得为空，长度受现有视频请求上限约束。

不调用默认文字模型，不把 assistant 的历史回复当作新的视频提示词。

### 4.2 文字内容

支持 OpenAI Chat Completions 常见两种形式：

- `content: "..."`；
- `content: [{"type":"text","text":"..."}, ...]`。

同一条 user 消息中的多个 text part 按原顺序用换行连接。暂不把 system、assistant、tool 消息隐式拼入提示词，避免把历史控制语句或工具输出误发给视频上游。

为了让客户端可以明确传视频参数，适配器允许请求顶层携带现有视频字段：

- `duration`；
- `resolution`；
- `ratio`；
- `image_asset_ids`；
- `video_asset_ids`。

这些字段仅在 `seedance` 分支解释，普通文字模型仍按原协议处理。

### 4.3 参考图片

支持 Chat Completions 的多模态图片 part：

```json
{
  "type": "image_url",
  "image_url": { "url": "data:image/png;base64,..." }
}
```

处理规则：

1. 只接受受大小、MIME 类型和解码结果校验通过的 data URL；
2. 网关在当前 Key/Principal 的素材所有者空间内创建临时 asset；
3. 生成的视频请求使用内部 `image_asset_ids`，再复用已有的原生素材上传逻辑；
4. 不接受外部电脑的 `C:\`、`D:\` 等本地路径；
5. MVP 拒绝任意 `http://` 或 `https://` 图片 URL，避免 SSRF、不可控下载和跨用户资源读取；客户端应先调用 `/v1/assets`，再把返回的 asset ID 放到 `image_asset_ids`；
6. `content` 中出现不支持的 part 类型时返回明确的 400，不静默丢弃图片或工具内容。

现有顶层 `image_asset_ids` / `video_asset_ids` 继续支持，最多沿用视频接口当前的数量和所有权校验。

### 4.4 生成的视频请求

适配器构造等价于以下内部请求，不通过本机 HTTP 再调用自己：

```json
{
  "model": "seedance",
  "prompt": "最后一条 user 消息的文字内容",
  "duration": 4,
  "resolution": "480p",
  "ratio": "16:9",
  "image_asset_ids": ["..."],
  "video_asset_ids": ["..."]
}
```

未提交的可选字段使用现有视频链路默认值；适配器不重复实现 Seedance 参数校验，而是调用现有 `video::validate_request` / 素材准备逻辑。

## 5. 权限、额度和并发

### 5.1 Legacy 模式

- `model=seedance` 分支需要 API Key 的 `video` 能力，而不是仅检查 `chat` 能力；
- 现有 `/v1/videos/generations` 的视频请求数、进行中任务数、素材上传限制和 Work 账号选择规则继续生效；
- 不计入文字 token 额度；真实 Work 积分由现有视频任务/上游结算链路负责；
- Key、任务和素材的所有权隔离保持不变。

鉴权中间件目前按路径先检查 `/v1/chat/completions` 的 `chat` 能力，因此适配器必须在路由内再次执行视频能力检查，不能允许“有 chat 无 video”的 Key 绕过视频权限。若请求同时需要内联图片素材，则还需通过既有素材写入权限路径。

### 5.2 Core enforce 模式

- `seedance` 分支需要 Principal 的 `videos:submit` scope；
- 不进入 `chat:invoke` 的文字租约、文字资源种类或文字 token 预留；
- 使用现有 `core_videos_generations` 的 video job、真实积分预留/结算、绑定账户、观测新鲜度、适配器 fail-closed 和视频并发控制；
- 内联 data URL 图片创建素材时，使用与当前 Principal 绑定的素材所有权和大小限制；
- 未配置显式视频适配器时，返回现有 scheduler/adapter 未启用错误，绝不回退到旧 ApiPool 或文字执行器。

为避免在 Core 中复用 `chat` 的估价和 scope，实施时应抽取一个共享的“已归一化视频提交”内部函数；不能通过伪造一个文字请求再调用 `preflight_chat_with_lease_for_accounts`。

## 6. 幂等和请求生命周期

- `seedance` 的 Chat 兼容入口要求 `Idempotency-Key`；Core enforce 与 Legacy 均要求，避免客户端重试导致重复扣除真实积分；
- 该键在 API Key/Principal 命名空间内隔离，复用现有视频幂等映射；
- 同一个键和相同投影请求返回同一视频任务；同一个键但投影内容不同返回冲突，不创建新任务；
- 幂等判定应使用去除敏感数据后的规范化视频请求哈希；不把原始 API Key 或完整提示词写入日志；
- 参考图 data URL 先完成确定性的校验/资产登记，再进入幂等提交，避免重复创建或重复上传素材；
- 上游已接受但本地响应中断时，任务保持可恢复状态，遵循现有视频 lease/settlement 语义。

## 7. 响应协议

Chat Completions 本身是同步文字协议，而 Seedance 是异步视频任务，因此不能伪造“视频已经完成”的文字完成响应。

MVP 采用：

- HTTP `202 Accepted`；
- `choices[0].message.content` 返回简短状态文本，例如“视频任务已提交，完成后可下载”；
- 顶层增加结构化 `video_task` 扩展，至少包含 `id`、`status`、`model` 和本地网关的状态/内容路径字段；
- 保留 `request_id`、`created` 等 OpenAI 兼容元数据；
- `finish_reason` 使用 `video_async`，不使用普通文字的 `stop`；
- 任务完成前不返回虚假的视频 URL；内容路径只有在任务完成且本地产物存在时才可用。

这里的任务字段是给通用 API 客户端做轮询/下载用的机器接口，不改变 MCP 高层工具的用户体验：MCP 仍在后台等待完成并将 MP4 自动下载到调用方电脑的 Downloads 文件夹，不向最终用户展示查询地址。此处不再采用旧的“直接把查询 URL 作为主要用户结果”的形式。

对于 `stream=true`：

- MVP 直接返回 400 `seedance_stream_unsupported`，要求客户端使用非流式 Chat 请求；
- 不发送伪造的 token SSE；
- 后续如有需要，可单独设计视频生命周期 SSE，但不放入本次改造。

## 8. 错误映射

适配器错误应保持 OpenAI error envelope，并使用稳定错误码：

- `seedance_model_requires_video_route`：模型字段或请求结构无法进入视频分支；
- `seedance_prompt_required`：没有可用的 user 文本；
- `seedance_unsupported_content_part`：出现不支持的内容 part；
- `seedance_inline_image_invalid`：data URL、MIME、Base64 或图片大小无效；
- `capability_not_allowed`：Key 没有 video 能力；
- `missing_scope`：Core Principal 没有 `videos:submit`；
- `idempotency_key_required`：缺少幂等键；
- `idempotency_conflict`：同一键对应不同请求；
- `video_limit_exceeded` / `quota_exceeded`：沿用视频限流和 Core 额度错误；
- `scheduler_endpoint_not_enabled` / `video_adapter_not_configured`：Core 视频适配器未配置；
- 上游已接受或传输不确定时，返回现有异步任务状态，不把不确定结果误报成未扣费。

## 9. 内部实现边界

建议新增一个只负责协议投影的模块，例如 `seedance_chat.rs`：

- `is_seedance_model`：模型规范化判定；
- `project_chat_to_video`：提取 prompt、参数和图片 part；
- `upload_inline_image`：将 data URL 交给现有 asset owner/store；
- `chat_video_response`：把统一视频任务结果包装成 Chat Completions 202 响应；
- `validate_idempotency_conflict`：复用/调用现有视频幂等哈希逻辑。

视频任务创建、Core lease、真实额度结算、上游执行和最终产物查询仍归 `video.rs` / `core_bridge.rs` / 现有视频路由负责。若为避免重复代码需要抽取函数，优先做小范围共享函数，不改动无关的文字路由。

## 10. 测试验收

测试全部使用 D 盘临时目录和 Fake/Mock adapter，不触发真实上游请求：

1. 普通模型 Chat 请求仍进入原文字执行器；
2. `seedance`、`Seedance`、带空白的模型名均进入视频适配器；
3. seedance 请求不会调用默认文字模型或文字 token 计费；
4. 字符串 content 能生成有效 prompt；
5. text parts 能按顺序拼接；
6. data URL 图片能创建并绑定当前所有者 asset；
7. 公网图片 URL、本地路径和不支持 part 被拒绝；
8. 顶层视频参数传递到视频校验/执行层；
9. 缺少 Idempotency-Key 被拒绝；
10. 相同幂等键重放同一任务，不重复创建、不重复执行；
11. 相同幂等键不同请求返回冲突；
12. Legacy 无 video 能力时拒绝，有 video 能力时使用视频限流；
13. Core 无 `videos:submit`、无视频适配器、无新鲜观测或额度不足时 fail closed；
14. Core 视频真实积分预留/结算路径与直接视频接口一致；
15. `stream=true` 返回明确错误；
16. 返回体为 HTTP 202 的 Chat envelope，未完成时不伪造 content URL；
17. 已完成任务的状态/内容下载仍可用，MCP 自动下载行为不回归；
18. 日志、错误和响应不泄露 API Key、凭证或完整内联图片数据。

## 11. 预计工时

在现有代码基础上：

- 适配器与共享视频提交抽取：60–90 分钟；
- 图片 part/asset 处理、幂等和错误映射：45–75 分钟；
- Legacy/Core 测试与回归：45–60 分钟；
- D 盘 Mock 构建验收：30–45 分钟。

合计约 3–4 小时。若现有 Core 视频提交函数可以无改动抽取共享入口，接近 3 小时；若 Core 测试夹具需要补齐，按 4 小时预留。期间不会自动消耗真实积分，也不会持续写入 C 盘。

## 12. 待确认的唯一协议选择

本规格推荐“HTTP 202 + `video_task` 机器扩展”，因为视频生成是异步的，通用 Chat 客户端必须有任务标识才能知道何时下载结果。若坚持 Chat Completions 必须等待视频完成后才返回，则需要把请求保持数分钟，并且不同客户端仍不一定能自动下载 MP4；这应作为另一个明确方案，不与本次 MVP 混合。

请确认采用本规格的推荐响应方式后，再进入实施计划和代码阶段。

# AI Work Assistant Phase 3B 素材归属与 Core 持久化设计

日期：2026-09-20
状态：实施基线
适用仓库：`E:\\AIWORK\\workspace\\TraeWorkAssistant`

## 1. 范围

本阶段只实现参考图片/参考视频素材的 Core enforce 路径，不改变 legacy off/shadow
路径，也不把本地文件路径或公网 URL 直接转交 Trae。目标是让素材元数据由 Core
SQLite 持久化、按用户而不是 API Key 隔离，并保持现有 `POST /v1/assets` JSON
上传契约及短时内容链接兼容。

本阶段不实现视频 jobs/attempts、支付、对象存储、multipart 上传或真实上游计费。
Trae 原生资源上传继续由显式绑定的 Tauri 适配器负责；Core 只管理用户资源和
安全的本地存储引用。

## 2. 安全与所有权不变量

- Core enforce 请求必须由 middleware 注入 `Principal`，请求体、查询参数和普通
  自定义请求头中的 `user_id` 不参与所有权判断。
- `assets.user_id` 是唯一业务所有者；`api_key_id` 只用于鉴权和审计，不写入资源
  所有权字段。
- Core 表保存 `storage_ref`、文件摘要和短时内容 token 的摘要，不保存 token 明文。
- `storage_ref` 只能是 `assets/<opaque-id>.<safe-extension>` 形状；通过目录解析后
  才能读取，拒绝 `..`、反斜线、绝对路径和符号链接逃逸。
- 创建流程先安全写入临时文件并原子改名，再在同一 Core 事务中写元数据；元数据
  失败时只清理本次新建文件。旧文件和旧 JSON 不被覆盖。
- `/v1/assets/<id>/content?token=...` 仍可免 API Key 访问，但必须匹配 Core 保存的
  token 摘要、资源未过期且文件摘要/大小校验通过；无效 token、跨用户猜测 ID 和
  过期资源统一返回 404。
- 资源字节数、格式、TTL 和请求体上限继续使用现有保守策略；不把素材存储额度
  与上游积分或用户生成额度混算。

## 3. 数据模型与迁移

从 schema v7 迁移到 v8，新增 `assets` 表：

| 字段 | 语义 |
| --- | --- |
| `id` | 稳定 opaque asset id，主键 |
| `user_id` | `users(id)` 外键 |
| `filename` / `mime_type` / `extension` | 已清洗的展示和响应元数据 |
| `size` / `sha256` | 实际文件大小和内容摘要 |
| `storage_ref` | 受约束的相对存储引用 |
| `content_token_digest` | 短时内容 token 的 SHA-256 摘要，唯一 |
| `created_at_ms` / `expires_at_ms` | UTC 毫秒时间 |
| `state` | `active` 或 `expired` |

新增按用户、状态、过期时间的索引。v7 已有的 `legacy_assets` 只用于迁移报告，
不自动把无法验证 owner 的记录写入 active `assets`；后续显式迁移工具再处理
映射记录。v8 迁移必须幂等，旧数据库失败时不得把 schema 版本标成 v8。

## 4. Rust 接口边界

`src-core` 提供不包含文件字节的 `CoreAsset` / `CreateAssetInput`，以及：

- `CoreStore::create_asset`：事务内校验用户、插入元数据和审计事件；拒绝重复 id
  或 token digest。
- `CoreStore::asset_for_user`：只按 `Principal.user_id` 返回元数据。
- `CoreStore::asset_by_content_token`：按 id、token digest、当前时间查找，过期不返回。
- `CoreStore::expire_assets`：只更新过期元数据状态，文件清理由 Tauri 本地清理器完成。

Tauri `assets` 模块新增 Core 专用文件读写辅助函数，但不复用 JSON index。legacy
路径继续使用现有 `create/find_owned/read_owned`，以降低回归风险。

## 5. 路由与错误契约

Core enforce：

- `POST /v1/assets` 要求 `assets:write`，成功返回现有 asset JSON，并可在配置了
  `AIWORK_ASSET_PUBLIC_BASE_URL` 时返回短时 `content_url`。
- 内容端点使用 Core token 校验；不要求 API Key，因此 Trae/远端客户端可以读取。
- 未配置 Core 运行时、缺少 scope、无效数据、超限、文件不存在和 token 失败分别
  映射为稳定的 501/403/400/413/404，不泄露用户、路径或 token。
- Core enforce 绝不回退到 legacy `assets.json`；旧索引中同名 id 不能影响 Core 查询。

## 6. 验收证据

必须使用 `D:\\gpt` 作为测试临时目录和 Cargo target，并清除/断言没有 `AIWORK_*`
环境变量。至少覆盖：schema v8 bootstrap 与 v7 migration、跨用户读取拒绝、token
摘要校验、过期拒绝、路径逃逸拒绝、创建失败清理、Core enforce route 的 scope/owner
边界，以及 legacy 测试不回归。不得调用真实上游或真实素材公网地址。

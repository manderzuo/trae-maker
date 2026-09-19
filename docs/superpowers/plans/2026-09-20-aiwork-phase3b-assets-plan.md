# Phase 3B 素材归属与 Core 持久化实施计划

> 目标：在不改变 legacy off/shadow 路径的前提下，把 `/v1/assets` 的 enforce 路径迁移到 Core 用户归属和本地安全存储。

## Task 1：先写 v8 schema 与 Core 资产模型

**文件：**

- 修改 `src-core/src/schema.rs`、`src-core/src/store.rs`
- 修改 `src-core/src/models.rs`、`src-core/src/lib.rs`
- 新增 `src-core/tests/assets_schema.rs`

**要求：**

- v7→v8 幂等迁移，active `assets` 表带用户外键、token digest 唯一约束、storage_ref 检查约束和过期索引。
- 提供创建、按用户查询、按内容 token 查询、过期标记方法；所有写入产生脱敏 audit 事件。
- 不把 `legacy_assets` 自动提升为 active 资产。

**先行测试：**新库 bootstrap、v7 migration、跨用户隔离、重复 token/id、过期和 storage_ref 拒绝。

## Task 2：Core 专用文件存储辅助

**文件：**

- 修改 `src-tauri/src/api_server/assets.rs`
- 新增/扩展单元测试

**要求：**

- 将字节写入临时文件并原子改名，返回 Core 所需 metadata 和随机 token；不更新 `assets.json`。
- 按 `storage_ref` 安全解析文件，验证大小和 SHA-256；拒绝目录穿越与符号链接逃逸。
- 保留 legacy `create/find_owned/read_owned/cleanup` 行为不变。

## Task 3：接入 Core enforce 路由

**文件：**

- 修改 `src-tauri/src/api_server/routes.rs`
- 必要时修改 `src-tauri/src/api_server/core_bridge.rs`
- 新增 Core asset route/smoke tests

**要求：**

- `/v1/assets` 使用 `Principal.user_id` 和 `assets:write`，不能从 body/header 取 owner。
- 内容端点在 Core mode 使用 token digest 查询；legacy mode 仍走旧 index。
- Core 缺少 scope、用户不活跃、文件/元数据不一致均 fail closed，不能回退 JSON。

## Task 4：验证、文档与提交

**要求：**

- 在 `D:\\gpt` 下运行 Core/Tauri focused 与全量回归，清除/断言 `AIWORK_*`。
- 更新素材运维说明和 `AGENT.md` 的测试约定。
- `git diff --check`，路径范围暂存，提交信息：`feat: persist user-owned assets in core`。

## 完成条件

Core v8 migration、资产 owner/token/storage 边界和 enforce route 有直接测试证据；
legacy 路径和用户已有未提交修改保持不变；没有真实上游调用、C 盘测试 target 或
未审查的敏感数据进入提交。

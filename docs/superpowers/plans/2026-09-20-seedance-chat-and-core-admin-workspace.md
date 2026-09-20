# Seedance Chat 兼容与 CORE 管理工作台实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `POST /v1/chat/completions` 在选择 `seedance` 时直接进入异步视频生产，同时提供一个独立的 CORE 管理工作台来控制用户、Key、scope、额度、任务和迁移。

**Architecture:** 先在网关内增加 Chat → Seedance 的纯协议投影层，再复用现有视频任务、素材、Core lease 和真实额度结算链路；不通过本机 HTTP 自调用。前端新增独立 CORE 工作区，使用现有 Tauri 管理命令，只在数据确实不足时增加最小 DTO/命令，不让前端直接读写 Core SQLite。

**Tech Stack:** Rust/Axum/Tauri 2、React 18、TypeScript、Vitest、Cargo 离线测试、D 盘临时目录与 Mock/Fake adapter。

**Spec:**

- `docs/superpowers/specs/2026-09-20-seedance-chat-completions-compatibility-design.md`
- `docs/superpowers/specs/2026-09-20-core-admin-workspace-design.md`

## Global Constraints

- 保留当前工作区已有未提交修改，不执行 reset、checkout、clean 或覆盖式清理。
- `model=seedance` 才进入视频适配器；其他 Chat Completions 请求保持原文字路由。
- Seedance Chat 分支不调用默认文字模型、不执行文字 token 计费，使用视频能力和真实视频积分。
- Core enforce 必须使用 `videos:submit`、视频资源账本、显式视频适配器和现有 fail-closed 规则。
- Chat → Seedance 的异步响应采用 HTTP 202 与 `video_task` 机器扩展；`stream=true` 返回明确错误，不伪造文字 SSE。
- 内联参考图只接受已校验的 data URL；公网 URL、本地路径和不支持的内容 part 拒绝。
- CORE 界面不展示 Key 明文、凭据、完整 prompt、本地结果路径或上游账号标识；管理员 Key 只保存在内存会话。
- 不提供 unknown/待对账视频任务的自动重放，不自动切换 Core 模式，不自动重启生产网关。
- 不向真实上游发送自动化测试请求；测试只使用 D 盘临时目录、Fake adapter 和本地 fixture。
- 测试、Cargo/npm 缓存和构建中间文件优先放在 `D:\gpt`，避免持续写入 C 盘。

## Review Focus

- 普通模型请求是否完全绕过 Seedance 适配器，且原文字路由与 token 计费不回归；对应 Task 2 的路由回归测试。
- Chat 的图片 part、Key/Principal 所有权和幂等键是否不会导致重复上传或重复扣费；对应 Task 3 的资产和幂等测试。
- Core 缺 scope、额度不足、观测过期或没有显式视频适配器时是否 fail closed；对应 Task 2 的 Core 测试。
- CORE 工作区刷新失败时是否保留旧数据显示错误，且管理员 Key/签发明文不会落盘；对应 Task 5/6 的 UI 测试。
- `held`、`settled`、`available` 和总量是否不混淆，unknown/待对账任务是否不出现重放按钮；对应 Task 6 的额度与任务测试。

---

### Task 1: 建立 Seedance Chat 投影层

**Files:**
- Create: `src-tauri/src/api_server/seedance_chat.rs`
- Modify: `src-tauri/src/api_server/mod.rs`
- Test: `src-tauri/src/api_server/seedance_chat.rs` 内 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: 已解析的 OpenAI Chat Completions `serde_json::Value`。
- Produces: `pub(crate) fn is_seedance_model(model: &str) -> bool`；`pub(crate) fn project_chat_to_video(body: &Value) -> Result<SeedanceChatProjection, SeedanceChatError>`。
- `SeedanceChatProjection` 至少包含 `video_input: Value`、内联图片列表和规范化请求哈希所需的稳定输入；不得保存 API Key 或完整敏感日志字段。

- [ ] **Step 1: 写失败测试**

  在 `seedance_chat.rs` 中先覆盖：模型大小写/空白归一化、字符串 content、text parts、最后一条 user 消息、空 prompt、缺少 messages、不支持 part、顶层 duration/resolution/ratio/image_asset_ids/video_asset_ids 传递。

  ```rust
  #[test]
  fn projects_last_user_message_without_calling_a_text_model() {
      let input = serde_json::json!({
          "model": " Seedance ",
          "messages": [
              {"role": "system", "content": "ignore"},
              {"role": "user", "content": "先前提示"},
              {"role": "assistant", "content": "历史回复"},
              {"role": "user", "content": [
                  {"type": "text", "text": "主体"},
                  {"type": "text", "text": "动作"}
              ]}
          ],
          "duration": 6
      });
      let projection = project_chat_to_video(&input).unwrap();
      assert_eq!(projection.video_input["model"], "seedance");
      assert_eq!(projection.video_input["prompt"], "主体\n动作");
      assert_eq!(projection.video_input["duration"], 6);
  }
  ```

- [ ] **Step 2: 运行定向测试确认失败**

  在 `D:\gpt` 设置 `TEMP`/`TMP` 后运行：

  ```powershell
  cargo test --manifest-path src-tauri/Cargo.toml api_server::seedance_chat --offline --locked
  ```

  Expected：因模块和投影函数尚未存在而失败。

- [ ] **Step 3: 实现最小投影器**

  实现以下稳定规则：

  ```rust
  pub(crate) fn is_seedance_model(model: &str) -> bool {
      model.trim().eq_ignore_ascii_case("seedance")
  }

  pub(crate) fn project_chat_to_video(
      body: &serde_json::Value,
  ) -> Result<SeedanceChatProjection, SeedanceChatError>;
  ```

  只读取最后一条 `role=user` 的字符串或 text parts；`image_url` 交给后续资产阶段；不调用任何文字执行器。复用现有视频参数范围，不复制一套新的 duration/resolution/ratio 规则。

- [ ] **Step 4: 运行定向测试确认通过**

  重跑 Task 1 的 Cargo 命令，Expected：投影、非法输入和字段保留测试全部 PASS。

- [ ] **Step 5: 提交独立变更**

  ```powershell
  git add src-tauri/src/api_server/seedance_chat.rs src-tauri/src/api_server/mod.rs
  git commit -m "feat: add seedance chat projection layer"
  ```

### Task 2: 接入 Legacy/Core 视频提交并包装 Chat 202 响应

**Files:**
- Modify: `src-tauri/src/api_server/routes.rs`
- Modify: `src-tauri/src/api_server/core_bridge.rs`（仅抽取共享视频提交接口时修改）
- Modify: `src-tauri/src/api_server/video.rs`（幂等输入哈希或任务返回值共享时修改）
- Test: `src-tauri/src/api_server/routes.rs` 现有 API 路由测试区

**Interfaces:**
- Consumes: `SeedanceChatProjection`、当前 `HeaderMap`、Legacy `ResolvedKey` 或 Core `Principal`。
- Produces: 一个内部 `VideoSubmission` 结果，至少能提供任务 ID、模型、状态、request ID 和是否幂等重放；现有 `/v1/videos/generations` 与 Chat 兼容入口共同使用它。
- `chat_completions` 在完成 JSON 解析后先调用 `is_seedance_model`；未命中分支必须保留现有代码路径。

- [ ] **Step 1: 写失败路由测试**

  增加测试覆盖：普通模型仍进入文字执行器；seedance 不调用文字执行器；Legacy 没有 `video` 能力时拒绝；Core 没有 `videos:submit` 时拒绝；缺少 `Idempotency-Key` 时拒绝；`stream=true` 返回 `seedance_stream_unsupported`；成功提交返回 202 Chat envelope 和 `video_task`。

  ```rust
  assert_eq!(response.status(), StatusCode::ACCEPTED);
  assert_eq!(payload["choices"][0]["finish_reason"], "video_async");
  assert_eq!(payload["video_task"]["model"], "seedance");
  assert!(payload["video_task"]["status"].as_str().is_some());
  ```

- [ ] **Step 2: 运行路由测试确认失败**

  ```powershell
  cargo test --manifest-path src-tauri/Cargo.toml api_server::routes --offline --locked
  ```

  Expected：新增 seedance Chat 测试失败，既有视频和文字测试保持可运行。

- [ ] **Step 3: 抽取共享视频提交入口**

  将现有 Legacy/Core 视频创建、幂等、视频并发、Core lease 和适配器调用抽取为同一组内部函数。Chat 入口只负责把请求投影成视频输入并选择正确的权限路径，不直接调用文字 preflight。

  具体行为：

  - Legacy 额外检查 `CAPABILITY_VIDEO`，使用视频限流和 Work 资源；
  - Core 只检查 `videos:submit`，调用现有 `core_videos_generations` 的视频账本/lease/adapter 路径；
  - 同一 API Key/Principal 下相同幂等键重放同一任务，不同投影返回冲突；
  - 任务未完成时不伪造视频完成 URL；
  - `choices[0].message.content` 只返回提交状态，顶层 `video_task` 供机器客户端读取；
  - 直接 `/v1/videos/generations` 的响应和行为保持兼容。

- [ ] **Step 4: 实现 Chat 分流与响应包装**

  在 `chat_completions` 中把 seedance 分支放在文字能力检查、文字 token 预留和 `dispatch::resolve_target` 之前；非 seedance 分支保持原代码。Legacy/Core 都使用非流式视频提交；`stream=true` 立即返回 400。

- [ ] **Step 5: 运行路由与 Core 定向测试**

  ```powershell
  $env:TEMP='D:\gpt'; $env:TMP='D:\gpt'
  cargo test --manifest-path src-tauri/Cargo.toml api_server::routes --offline --locked
  cargo test --manifest-path src-tauri/Cargo.toml api_server::core_bridge --offline --locked
  ```

  Expected：新增测试和既有 Chat/Video/Core 测试全部 PASS；测试不得访问真实上游。

- [ ] **Step 6: 提交独立变更**

  ```powershell
  git add src-tauri/src/api_server/routes.rs src-tauri/src/api_server/core_bridge.rs src-tauri/src/api_server/video.rs
  git commit -m "feat: route seedance through chat completions"
  ```

### Task 3: 接入 Chat 多模态图片与资产所有权

**Files:**
- Modify: `src-tauri/src/api_server/seedance_chat.rs`
- Modify: `src-tauri/src/api_server/assets.rs`（只复用或补充 owner-scoped 写入 helper）
- Modify: `src-tauri/src/api_server/routes.rs`
- Test: `src-tauri/src/api_server/seedance_chat.rs`、`src-tauri/src/api_server/assets.rs`

**Interfaces:**
- Consumes: `image_url.url` data URL、当前 API Key/Principal、D 盘临时数据目录。
- Produces: 已绑定调用方所有者的 `image_asset_ids`，供现有 `video::prepare_native_request` 使用。

- [ ] **Step 1: 写失败测试**

  覆盖 PNG/JPEG data URL、非法 Base64、未知 MIME、超大图片、公网 URL、本地路径、不支持 part、不同 Key 不能读取他人 asset。

- [ ] **Step 2: 运行测试确认失败**

  ```powershell
  cargo test --manifest-path src-tauri/Cargo.toml api_server::seedance_chat api_server::assets --offline --locked
  ```

- [ ] **Step 3: 实现 data URL 校验与 owner-scoped asset 创建**

  MIME、解码结果、单图和总图大小使用现有素材限制；创建后只把内部 asset ID 放入视频投影，不把 Base64 或本地路径写入任务 prompt/日志。公网 URL 和本地路径返回稳定错误码。

- [ ] **Step 4: 运行测试确认通过**

  使用 `TEMP=D:\gpt`、`TMP=D:\gpt` 重跑 Task 3 命令，并确认测试目录只在 D 盘产生。

- [ ] **Step 5: 提交独立变更**

  ```powershell
  git add src-tauri/src/api_server/seedance_chat.rs src-tauri/src/api_server/assets.rs src-tauri/src/api_server/routes.rs
  git commit -m "feat: support inline images for seedance chat"
  ```

### Task 4: 补齐 CORE 工作台前端契约与纯函数

**Files:**
- Modify: `src/types.ts`
- Modify: `src/lib/tauri.ts`
- Create: `src/components/core/coreModel.ts`
- Test: `src/components/core/coreModel.test.ts`
- Modify: `src-tauri/src/commands/core.rs`（仅当 migration DTO 当前无法由前端安全使用时）

**Interfaces:**
- Produces: 与 Rust `MigrationReport`、`MigrationApplyResponse`、`LegacyMigrationMapping` 对齐的 TypeScript 类型；`api.core.migrationInspect`、`api.core.migrationApply`；额度计算函数 `coreQuotaSummary(balance)`。
- `coreQuotaSummary` 返回 `{ total, available, held, settled }`，计算为 `total=available+held+settled`，不把 `held` 当作已使用。

- [ ] **Step 1: 写失败测试**

  ```ts
  it('keeps available, held and settled separate', () => {
    expect(coreQuotaSummary({ available: 40, held: 10, settled: 50 })).toEqual({
      total: 100,
      available: 40,
      held: 10,
      settled: 50,
    });
  });
  ```

  同时测试 scope 去重、迁移状态 tone、API Key 明文不进入列表类型。

- [ ] **Step 2: 运行前端定向测试确认失败**

  ```powershell
  npm test -- --run src/components/core/coreModel.test.ts
  ```

- [ ] **Step 3: 实现类型、Tauri wrapper 和纯函数**

  复用已注册的 Core 命令；不新增前端 SQLite 读取。migration 操作仍通过 Rust 管理命令和管理员鉴权执行。

- [ ] **Step 4: 运行前端测试确认通过**

  ```powershell
  npm test -- --run src/components/core/coreModel.test.ts
  ```

- [ ] **Step 5: 提交独立变更**

  ```powershell
  git add src/types.ts src/lib/tauri.ts src/components/core/coreModel.ts src/components/core/coreModel.test.ts
  git commit -m "feat: add core console data contracts"
  ```

### Task 5: 建立独立 CORE 工作区壳层与管理员会话

**Files:**
- Create: `src/pages/CoreConsole.tsx`
- Create: `src/components/core/CoreWorkspace.tsx`
- Create: `src/components/core/CoreStatusBar.tsx`
- Create: `src/components/core/CoreNav.tsx`
- Create: `src/components/core/useCoreAdminSession.ts`
- Modify: `src/types.ts`
- Modify: `src/components/Sidebar.tsx`
- Modify: `src/App.tsx`
- Modify: `src/components/api/ApiManagerModal.tsx`
- Test: `src/components/core/CoreWorkspace.test.tsx`

**Interfaces:**
- `useCoreAdminSession()` 提供 `adminApiKey`、`hasAdminKey`、`loading`、`error`、`status`、`scheduler`、`users`、`keys`、`videoJobs`、`loadAll()`、`clearSession()`。
- `CoreConsole` 只负责页面布局；各数据分区通过 props 或 session hook 获取数据，不在组件内重复实现鉴权。

- [ ] **Step 1: 写失败 UI 测试**

  测试独立入口可渲染、无 Key 时只显示输入和状态、输入 Key 后加载按钮可用、清除会话后输入和敏感数据清空、刷新失败仍保留旧列表并显示错误。

- [ ] **Step 2: 运行测试确认失败**

  ```powershell
  npm test -- --run src/components/core/CoreWorkspace.test.tsx
  ```

- [ ] **Step 3: 实现工作区壳层**

  在 `ViewKey` 增加 `core-console`，在侧边栏工具区增加“CORE 管理”入口，点击后关闭 API 管理弹窗并进入独立页面。保留 API 管理弹窗中的 Core 入口作为跳转入口，不再把旧大面板作为唯一入口。

  顶部状态栏固定显示 Core 模式、网关状态、schema、调度健康摘要和刷新；管理员 Key 仅放 React state，不写 localStorage、URL、配置文件或日志。

- [ ] **Step 4: 运行测试确认通过**

  ```powershell
  npm test -- --run src/components/core/CoreWorkspace.test.tsx
  npm run test:buttons
  ```

- [ ] **Step 5: 提交独立变更**

  ```powershell
  git add src/pages/CoreConsole.tsx src/components/core/CoreWorkspace.tsx src/components/core/CoreStatusBar.tsx src/components/core/CoreNav.tsx src/components/core/useCoreAdminSession.ts src/types.ts src/components/Sidebar.tsx src/App.tsx src/components/api/ApiManagerModal.tsx src/components/core/CoreWorkspace.test.tsx
  git commit -m "feat: add standalone core admin workspace"
  ```

### Task 6: 实现 CORE 五个管理分区

**Files:**
- Create: `src/components/core/CoreOverviewPanel.tsx`
- Create: `src/components/core/CoreUsersKeysPanel.tsx`
- Create: `src/components/core/CoreQuotaPanel.tsx`
- Create: `src/components/core/CoreJobsPanel.tsx`
- Create: `src/components/core/CoreMigrationPanel.tsx`
- Modify: `src/components/core/CoreWorkspace.tsx`
- Modify: `src/components/core/coreModel.ts`
- Test: `src/components/core/CoreAdminPanels.test.tsx`

**Interfaces:**
- Panels receive session data and mutation callbacks，不直接持有管理员 Key。
- `CoreUsersKeysPanel` 使用可读 scope 多选，提交 `string[]`，签发结果通过一次性弹层展示 plaintext。
- `CoreQuotaPanel` 使用 `coreQuotaSummary`，提供用户余额、Key 余额、用户发放、Key 发放和 legacy 迁移确认动作。
- `CoreJobsPanel` 只显示后端允许的脱敏字段，unknown/待对账状态不渲染重放按钮。
- `CoreMigrationPanel` 先 inspect，再按报告允许状态启用 apply；apply 后刷新用户、Key 和额度。

- [ ] **Step 1: 写失败测试**

  覆盖：

  - 用户/Key 表格筛选和状态徽标；
  - scope 多选生成 `chat:invoke`、`videos:submit` 等准确数组；
  - Key plaintext 只出现在 issued modal，不出现在列表；
  - 额度卡片显示 total/available/held/settled 四个独立值；
  - 发放、迁移、撤销、禁用前必须出现确认内容；
  - unknown/reconcile_required 任务无重放按钮；
  - migration inspect 未通过时 apply 不可用。

- [ ] **Step 2: 运行测试确认失败**

  ```powershell
  npm test -- --run src/components/core/CoreAdminPanels.test.tsx
  ```

- [ ] **Step 3: 实现面板**

  按 CORE 规格拆分五个分区，维持统一的刷新、错误、确认和 toast 行为。危险操作使用明确的对象、资源类型、数量、原因和不可逆提示。

- [ ] **Step 4: 运行测试确认通过**

  ```powershell
  npm test -- --run src/components/core/CoreAdminPanels.test.tsx
  npm run test:buttons
  ```

- [ ] **Step 5: 提交独立变更**

  ```powershell
  git add src/components/core
  git commit -m "feat: add core users quota jobs and migration panels"
  ```

### Task 7: 全量验证、文档和发布前检查

**Files:**
- Modify: `docs/mcp-seedance.md`
- Modify: `docs/superpowers/specs/2026-09-20-seedance-chat-completions-compatibility-design.md`（仅记录已确认的响应协议）
- Modify: `docs/superpowers/specs/2026-09-20-core-admin-workspace-design.md`（仅记录已确认的入口名称）
- Test: `src-tauri` Cargo tests、前端 Vitest/build/button checks

- [ ] **Step 1: 在 D 盘建立隔离临时目录并设置测试环境**

  ```powershell
  $testRoot = 'D:\gpt\aiwork-seedance-core-acceptance-20260920'
  New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
  $env:TEMP = 'D:\gpt'
  $env:TMP = 'D:\gpt'
  ```

- [ ] **Step 2: 运行 Rust 定向和全量离线测试**

  ```powershell
  cargo test --manifest-path src-tauri/Cargo.toml api_server::seedance_chat --offline --locked
  cargo test --manifest-path src-tauri/Cargo.toml api_server::routes --offline --locked
  cargo test --manifest-path src-tauri/Cargo.toml api_server::core_bridge --offline --locked
  cargo test --manifest-path src-tauri/Cargo.toml --offline --locked
  ```

  Expected：所有测试通过，且无真实上游网络请求。

- [ ] **Step 3: 运行前端测试和构建**

  ```powershell
  npm test -- --run
  npm run test:buttons
  npm run build
  ```

- [ ] **Step 4: 做人工 UI 验收**

  验收独立 CORE 入口、管理员会话清除、状态刷新失败保留数据、Key scope 多选、额度四项数值、危险操作确认、unknown 任务无重放按钮，以及 Seedance Chat 的 202 响应展示。

- [ ] **Step 5: 检查变更边界**

  ```powershell
  git diff --check
  git status --short
  ```

  确认没有删除用户文件、没有持续写入 C 盘、没有覆盖既有 dirty 修改、没有把真实 Key 写入日志或测试产物。

- [ ] **Step 6: 交付说明**

  汇总变更文件、测试结果、CORE 工作台入口、Seedance Chat 调用方式和已知异步限制；不自动重启生产进程，不重新创建或修改后台监控。

## 预计工时

- Seedance Chat 兼容与图片/幂等：约 3–4 小时；
- CORE 独立工作台：约 2–3 小时；
- 集成验证与人工验收：约 45–60 分钟。

总计约 6–8 小时。若只做现有 Core 后端命令的前端重排，接近 6 小时；若需要补充 migration DTO、额度汇总接口或 Core 测试夹具，按 8 小时预留。此估算不包含真实公网视频生产等待时间。

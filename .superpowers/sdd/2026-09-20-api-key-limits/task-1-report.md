# Task 1 实现报告：Key 策略数据模型与向后兼容持久化

## 修改文件

- `src-tauri/src/api_server/api_keys.rs`
  - 新增 `KeyLimits`、`KeyCapabilities` 与能力常量。
  - 为 `ApiKeyEntry` 增加 `limits`、`capabilities` 的 Serde 默认值。
  - 为 `ResolvedKey` 增加策略快照字段并实现安全序列化；不包含 Key 明文。
  - 增加限额归一化：继承型字段使用 `Option`，每日请求/Token 字段使用数值且 `0` 表示不限；负数由无符号反序列化拒绝，超范围值钳制到安全上限。
  - 缺失 capability 列表兼容为 `chat`、`video`、`assets` 全能力；保留原有 `daily_limit`、`used_today` 和每日统计记账逻辑。
  - 新增简报要求的三组模块测试。
- `src/types.ts`
  - 新增 `KeyCapability`、`KeyCapabilities`、`KeyLimits`。
  - 为 `ApiKeyEntry` 补充可选的 `limits`、`capabilities` 字段，以兼容现有旧形状创建调用；后端对缺失字段补默认值。
- `.superpowers/sdd/2026-09-20-api-key-limits/task-1-report.md`
  - 本报告。

## 设计决定

- `KeyLimits` 的覆盖字段沿用现有全局限流配置命名：`max_inflight`、`asset_uploads_per_minute`、`asset_bytes_per_hour`、`video_submissions_per_minute`；缺失值为 `None`，交由后续限流器继承全局配置。
- 每日请求和 Token 限额分别使用 `daily_requests`、`daily_tokens`，数值 `0` 保留为不限语义。
- capability 列表只保留 `chat`、`video`、`assets`，显式空列表仍表示无能力；仅缺失字段才按向后兼容规则默认为全能力。
- `ResolvedKey` 只携带策略快照，不携带 `ApiKeyEntry.key`；其序列化测试显式检查不存在 Key 字段。
- 未改动现有每日配额字段和统计行为，也未改动工作区中其他任务或用户已有文件。

## 测试命令及结果

Cargo 测试使用 `CARGO_TARGET_DIR=D:\gpt\aiwork-key-limits-cargo-target`；依赖使用已有本地缓存并以 `--offline` 运行，未在 `C:` 创建持续测试输出。

1. RED：

   `cargo test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_keys -- --nocapture`

   新测试先于生产模型实现执行，按预期因 `KeyLimits`、策略字段和序列化实现缺失而编译失败。

2. GREEN：

   同一 focused 命令通过：`12 passed; 0 failed`。

3. 简报要求的现有 API Key 测试：

   `cargo test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_keys`

   `12 passed; 0 failed`。

4. 前端类型检查：

   `node_modules\\.bin\\tsc.cmd --noEmit`

   通过。

5. 前端测试：

   `node_modules\\.bin\\vitest.cmd run`

   `5 files passed; 29 tests passed`。

6. 全 Rust 测试套件：

   `cargo test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml`

   `461 passed; 4 ignored; 3 failed`。失败均为未改动的 `core_migration` 资产迁移测试，见下方未解决问题。

## 未解决问题 / concerns

- 全 Rust 套件中既有的以下三个 `core_migration` 测试仍失败：
  - `apply_does_not_copy_legacy_plaintext_to_core_or_report`
  - `apply_rejects_legacy_asset_hash_mismatch_without_writing`
  - `remaining_credit_history_is_observation_only_and_preserves_summary_fields`
  失败信息指向 legacy asset 文件缺失或 hash 校验预期，与本任务 Key 模型改动无关；按文件范围要求未修改相关文件。
- 当前 Rust 工具链未安装 `rustfmt` 组件，因此无法执行格式检查命令；编译、focused/full 测试和 `git diff --check` 已完成。
- Cargo 初始联网解析受不可用本地代理阻断，后续使用已有本地依赖缓存离线完成验证。

## 修复轮次 1（审查反馈）

### 处理内容

- 在 `KeyLimits` 中增加 `max_video_jobs: Option<usize>`，上限归一化与其他继承型并发字段一致；旧 JSON 缺失时默认为 `None`。
- `ResolvedKey` 通过其 `limits` 策略快照携带 `max_video_jobs`；TypeScript `KeyLimits` mirror 同步增加 `number | null` 字段。
- 保留 `KeyCapabilities = Vec<String>`，因为能力集合以字符串落盘且后续可能扩展；不采用会让旧/未来值反序列化失败的 Rust 枚举。通过同一归一化入口过滤未知值并去重，反序列化、认证快照和 `save` 都执行；显式空列表仍保持空，不被当作旧字段缺失。
- 增加非空 legacy Key 认证测试、显式空 capability 列表保存/读取测试、能力归一化持久化测试，以及 `max_video_jobs` 的默认、上限和 ResolvedKey 传递断言。

### 修复轮次测试命令及结果

1. RED：

   `cargo test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_keys -- --nocapture`

   新测试先运行，因 `max_video_jobs` 字段和安全上限常量缺失而编译失败。

2. GREEN focused：

   同一命令通过：`15 passed; 0 failed`；Cargo target 仍为 `D:\gpt\aiwork-key-limits-cargo-target`。

3. 前端复核：

   `node_modules\\.bin\\tsc.cmd --noEmit`：通过。

   `node_modules\\.bin\\vitest.cmd run`：`5 files passed; 29 tests passed`。

4. 完整 Rust 套件：

   `cargo test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml`

   `464 passed; 4 ignored; 3 failed`。仍是原有三个 `core_migration` legacy asset/hash 测试，失败位置和原因未变；无新增失败。

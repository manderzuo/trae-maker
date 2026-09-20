# Task 3 修复轮次 1：子任务 A 收尾报告

日期：2026-09-20  
范围：额度日期、认证策略快照、文字请求额度一致性  
状态：未完成（保留已有改动，停止继续扩展）

## 结论

本轮保留工作区中已有的 UTC 日期切换、认证阶段 `ResolvedKey` 注入，以及 Key 额度记录入口的 UTC 日期改动；未继续实现 token reservation/settlement、legacy handler 快照消费、统一 quota 错误和 Chat Completions 计数修复。因此不能把子任务 A 标记为完成，也没有声称严格 429 上限。

没有修改视频幂等、视频 permit 或 Core 并发逻辑。工作区已有的相关改动保持原样，未作 reset、checkout 或 clean。

## 按审查发现逐条映射

### 1. Key Token quota 日期必须使用 UTC（审查发现 2）

状态：部分完成，已有改动保留。

- `src-tauri/src/api_server/usage.rs:225` 的 `today_key()` 继续保留本地日期语义，供旧报表/历史展示使用。
- `src-tauri/src/api_server/usage.rs:230` 新增 `key_quota_day()`，使用 UTC 日期作为 Key quota 统计日期。
- `src-tauri/src/api_server/auth.rs:108`、`src-tauri/src/api_server/mod.rs:193`、`src-tauri/src/api_server/mod.rs:224` 已切换到 `key_quota_day()`。
- 现有 UTC focused 测试通过，但测试是“当前 UTC 时间对当前 UTC 时间”的比较，没有固定时钟覆盖跨本地日/UTC 日边界；这是后续仍应补强的测试 concern。

### 2. 认证产生的 policy snapshot 必须被 legacy handler 使用（审查发现 7）

状态：未完成；这是明确的源码错误。

- `src-tauri/src/api_server/auth.rs:120-123` 已把认证得到的 `ResolvedKey` 放入 request extensions，这部分改动保留。
- `src-tauri/src/api_server/routes.rs:121-145` 的 `legacy_policy`/`require_legacy_capability` 仍按 `key_id` 调用 `api_keys::constraints_for` 重新读文件；找不到时回退 `KeyLimits::default()`。
- 另外仍有重新读取入口：`src-tauri/src/api_server/routes.rs:2817`，以及 `src-tauri/src/api_server/wb_route.rs:227`、`473`、`754`。

这意味着认证成功后的策略快照可能被文件重读、删除或改坏的结果覆盖，且可能错误回退默认限额。

### 3. Token quota 检查、计数、结算必须一致（审查发现 4）

状态：未完成；这是明确的源码错误。

- `src-tauri/src/api_server/api_keys.rs:292` 的 `quota_left` 只检查已持久化的完成 token 统计，没有 pending reservation。
- `src-tauri/src/api_server/api_keys.rs:385-430` 的请求消耗与 `src-tauri/src/api_server/api_keys.rs:496-514` 的 token 记录是分离操作。
- `src-tauri/src/api_server/mod.rs:171-224` 仍直接把 usage 写入统计，当前没有按 Key 的 reservation/settlement 状态或失败回收路径。

因此并发请求可能同时通过 daily token 检查并越过上限。未知 usage 当前没有伪造为已知 token，但也没有对应的 reservation 生命周期；在失败/未知路径上无法证明 reservation 不泄漏。由于本轮没有实现该机制，代码也没有把它描述成严格上限。

### 4. quota 错误字段/消息和 Chat Completions 一次计数（审查发现 8、9）

状态：未完成；以下是明确源码错误位置。

- `src-tauri/src/api_server/auth.rs:268-280` 的 `quota_exceeded` 使用 `type: "quota_exceeded"`、`code: "daily_quota_exceeded"`，消息固定写“次数/日”，不能准确表达 token quota，也没有与其他 quota 路径统一字段/消息。
- `src-tauri/src/api_server/routes.rs:928` 的 `chat_completions` 当前没有与其他文字 handler 对等的 `state.total_requests.fetch_add(1, ...)`；该处理会漏记请求，而不是保证一次请求一次计数。

本轮没有继续改动这些位置，因此不宣称 quota 错误或文字请求计数已经修复。

## RED / GREEN 记录

### RED（回归测试草稿，随后按收尾要求移除）

使用离线依赖、独立 Cargo target 和单一测试过滤器运行：

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_quota_reservation_blocks_a_second_inflight_request --offline
```

结果：预期编译失败。草稿测试要求尚不存在的 `reserved_tokens`、`settle_token_reservation_locked`、固定 UTC 日期 helper，以及 legacy snapshot 参数；因此证明当前源码尚未具备 reservation/settlement 和 snapshot 消费实现。草稿测试已移除，没有把不可编译测试留在工作区。

### GREEN（现有 focused 回归）

以下每次均使用 `D:\gpt\aiwork-key-limits-cargo-target`、`C:\Users\StarLink\.cargo`、Rust toolchain bin PATH 和 `--offline`，每次只有一个测试过滤器：

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml key_quota_day_uses_utc_date --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml daily_token_quota_blocks_after_recorded_usage_reaches_limit --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml quota_blocks_at_limit_and_resets_next_day --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml locked_verify_skips_write_when_unchanged --offline
```

结果：通过，1 passed，0 failed。

测试过程只有已有的 Rust unused/dead-code 警告和 Windows linker PDB/default-library 警告，没有测试失败；没有在 C: 写持续测试输出，也没有输出 Key 明文。

## 文件理由与变更边界

- `src-tauri/src/api_server/usage.rs`：保留 UTC quota 日期 helper，同时保留旧报表本地日期 helper；已有 focused UTC 测试通过。
- `src-tauri/src/api_server/auth.rs`：保留认证阶段 `ResolvedKey` 注入和 UTC quota 日期调用；quota 错误统一尚未完成，错误位置已列出。
- `src-tauri/src/api_server/mod.rs`：保留 usage 记录入口使用 UTC quota 日期；reservation/settlement 尚未实现。
- `src-tauri/src/api_server/api_keys.rs`：本轮没有新增生产改动；现有非原子 quota 路径是待修复错误位置。
- `src-tauri/src/api_server/routes.rs`、`wb_route.rs`：本轮没有继续扩展；现有 handler 重读策略和 Chat Completions 漏计数是待修复错误位置。

## Git 状态

没有提交本轮未完成的生产实现；本次只提交这份报告（docs-only commit）。工作区其他既有修改均未清理、未回退、未混入本子任务提交。

## Concerns

1. 在完成按 Key reservation/settlement 前，daily token quota 只能视为非原子软检查，不能承诺严格上限或严格 429。
2. 完成 snapshot 改造时必须覆盖 `routes.rs`、`wb_route.rs` 等所有 legacy 入口，不能只改一个 helper。
3. 下一轮若实现严格 reservation，应明确未知 usage 的释放/结算语义，并补并发、失败、未知 usage 和跨 UTC 日边界测试。

---

# Task 3 修复轮次 2：A2.1 legacy 文字 Token reservation 接入

日期：2026-09-20
范围：legacy chat/completions、responses、messages、Anthropic/OpenAI Text、WB/custom 文字请求的 Token reservation 生命周期
状态：完成（A2.1）

## 实现

- `routes.rs` 新增文字专用 guard：先取得普通 request permit，再按当前 `KeyLimits.daily_tokens` 调用 `reserve_token_quota`；reservation 失败返回既有 `quota_exceeded` 429，并显式释放 request permit。anonymous 与 `daily_tokens=0` 不创建 reservation；视频/assets 仍使用原 request guard。
- Trae、WB、custom 文字成功路径统一通过 `record_usage_with_guard`（custom 使用对应变体）提交已知 token 并结算；失败、未知或 0 usage 保持由 `InflightGuard::Drop` 释放 reservation。
- 新增并发 reservation、guard 成功结算、失败释放、quota rejection 释放 permit，以及 anonymous/无限额度不建 reservation 的 focused 测试。
- 修正 routes 测试中对当前 `require_legacy_capability`、`chat_completions` 生产签名的调用点；不改视频、Core 或 Chat 计数生产逻辑。

## focused 测试

以下命令均使用 `D:\gpt\aiwork-key-limits-cargo-target`、`C:\Users\StarLink\.cargo`、Rust toolchain PATH 和 `--offline`，每次只使用一个 Cargo 过滤器：

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_text_quota_rejection_releases_the_request_permit --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_handlers_use_the_persisted_policy_without_defaulting_authenticated_keys --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_text_guard_skips_reservation_for_anonymous_and_unlimited_keys --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml concurrent_token_reservations_allow_only_one_request_for_remaining_budget --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml t04_guard_settles_known_token_usage_once --offline
```

结果：通过，1 passed，0 failed。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml t05_guard_drop_releases_unknown_token_usage --offline
```

结果：通过，1 passed，0 failed。

`cargo check --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml --offline` 通过。格式检查未执行：当前 toolchain 未安装 rustfmt。

## Concern

`chat_completions_counts_one_authenticated_request_without_duplicate_increment` 的调用点已按当前 handler 签名修正，但单独运行时仍断言失败（实际计数为 0）；该测试对应 Chat 计数子任务，按本轮边界未修改生产计数逻辑。

除已有 Rust unused/dead-code 和 Windows linker PDB/default-library 警告外，A2.1 focused 测试无失败。

---

# Task 3 修复轮次 3：A2.1 review findings fix round 1

日期：2026-09-20
范围：失败尝试 Token 结算隔离、固定 UTC 日期 reservation 清理与结算归属
状态：本轮两项修复完成；全 Rust suite 保留 4 个前置无关失败

## 修复说明

1. `ApiSharedState::record_usage_with_guard` 及 custom 变体现在只有 `settle=true` 的最终成功路径才把 usage 放入 `InflightGuard` 的 observed Token 并结算。失败/中间重试即使带有已知 usage，也只记录普通请求用量；guard Drop 释放 reservation，不会把失败 Token 合并到后续成功请求。
2. `reserve_token_quota` 在新固定 UTC 日清理旧 reservation 后，即使新日额度已满而拒绝预留，也会把清理结果持久化。
3. 结算策略明确为 reservation-date：Token 记入 reservation 保存的 UTC 日期，不随终态 UTC 日期漂移。现有终态日期参数保留兼容，但不参与记账；新增固定 `2026-09-19` reservation / `2026-09-20` terminal 测试。

没有修改认证 snapshot、Chat 计数、quota 错误、Core、视频或视频 permit 逻辑。

## RED 记录

测试环境统一使用 `D:\gpt\aiwork-key-limits-cargo-target`、`C:\Users\StarLink\.cargo`、Rust toolchain PATH 和 `--offline`；每条 Cargo 命令只有一个测试过滤器。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml t05_failed_known_usage_is_not_merged_into_later_successful_settlement --offline
```

结果：失败，实际 prompt 为 4，预期 1；证明失败尝试 usage 被合并。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_cleanup_persists_when_new_fixed_utc_day_is_exhausted --offline
```

结果：失败，旧日期 reservation 仍存在于持久化文件；证明拒绝路径没有保存清理。

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_settles_on_reservation_date_not_terminal_date --offline
```

结果：失败，实际日期为 `2026-09-20`，预期 reservation 日期 `2026-09-19`；证明结算归属未明确实现。

## GREEN / 回归结果

以下三条修复回归均通过，分别为 1 passed，0 failed：

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml t05_failed_known_usage_is_not_merged_into_later_successful_settlement --offline
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_cleanup_persists_when_new_fixed_utc_day_is_exhausted --offline
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_settles_on_reservation_date_not_terminal_date --offline
```

同一环境下复核的既有 reservation/A2.1 focused 过滤器也全部通过：

```text
t04_guard_settles_known_token_usage_once
t05_guard_drop_releases_unknown_token_usage
token_reservation_blocks_a_second_request_and_settles_known_usage
token_reservation_releases_unknown_usage_without_leaking_capacity
concurrent_token_reservations_allow_only_one_request_for_remaining_budget
legacy_text_quota_rejection_releases_the_request_permit
legacy_text_guard_skips_reservation_for_anonymous_and_unlimited_keys
legacy_handlers_use_the_persisted_policy_without_defaulting_authenticated_keys
```

每个过滤器均为 1 passed，0 failed。完整 Rust 测试命令：

```text
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml --offline
```

结果：487 passed，4 failed，4 ignored。失败均为前置工作区已有范围：

- `api_server::routes::tests::chat_completions_counts_one_authenticated_request_without_duplicate_increment`：实际计数仍为 0，属于报告中已知的 Chat 计数子任务。
- `core_migration::tests::apply_does_not_copy_legacy_plaintext_to_core_or_report`：`legacy asset file is missing`。
- `core_migration::tests::apply_rejects_legacy_asset_hash_mismatch_without_writing`：未得到 hash 错误。
- `core_migration::tests::remaining_credit_history_is_observation_only_and_preserves_summary_fields`：`legacy asset file is missing`。

这些失败不在本轮允许范围，未修改对应逻辑。测试输出只有已有 unused/dead-code 和 Windows linker 警告。

## 本轮 concerns

1. 完整 suite 仍受上述 4 个前置失败影响，不能宣称全量测试全绿；本轮相关 focused 测试全绿。
2. reservation-date 策略要求 reservation 在结算时仍存在；若进程重启或跨日清理已移除旧 reservation，现有旧 lease 仍按既有 no-op 语义处理，不在本轮扩展恢复/对账生命周期。
3. 当前工作区仍有前置 UTC、认证策略、Core 等未提交改动；提交时只暂存本轮两个实现、三条回归测试及本报告追加内容。
---

# Task 3 修复轮次 4：A2.2a 最小收尾

日期：2026-09-20
范围：Token reservation 跨 UTC 日生命周期、legacy auth policy snapshot/quota error、Chat Completions 单次请求计数
状态：本轮范围完成；Core request permit、视频幂等原子性、视频 permit 回收未处理

## 改动

1. reservation 按创建时 UTC 日期持久化，跨日运行中的 reservation 保留到显式 settle/release；只有当前 UTC 日 reservation 参与当前日额度占用。daily_stats 在写入前统一排序、同日合并并限制 90 天，旧日期结算不会追加重复或乱序记录。
2. legacy handler 直接消费鉴权阶段注入的 `ResolvedKey` 快照；匿名请求明确使用默认策略；非匿名请求缺失或不匹配快照返回 `500 auth_snapshot_missing`，不会回读持久化 store 或静默放宽策略。WB legacy 三条路径和图片/素材/视频 legacy 路径均传递同一快照。
3. quota 超限响应统一为 `error.type/code = quota_exceeded`，保留 `legacy_code = daily_quota_exceeded`、`param` 和原有 message/status 兼容字段。
4. Chat Completions 在 legacy/Core 分流前统一递增一次 `total_requests`；未新增其它文字路径计数。

## 改动文件

- `src-tauri/src/api_server/api_keys.rs`
- `src-tauri/src/api_server/auth.rs`
- `src-tauri/src/api_server/routes.rs`
- `src-tauri/src/api_server/wb_route.rs`
- `src-tauri/src/api_server/phase3_streaming_smoke.rs`（仅为新增可选 auth snapshot 参数更新既有 Core 测试调用）
- 本报告文件

## 测试环境与结果

命令使用 `CARGO_HOME=C:\Users\StarLink\.cargo`、`CARGO_TARGET_DIR=D:\gpt\aiwork-key-limits-cargo-target`、stable toolchain PATH、`--offline`；没有把 Cargo/test 产物写入 C 盘。

已完成的聚焦命令均返回成功：

```text
cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_ --offline -- --nocapture
结果：6 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml quota_error_keeps_compatible_fields_and_identifies_token_limit --offline -- --nocapture
结果：1 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_text_quota_rejection_releases_the_request_permit --offline -- --nocapture
结果：1 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_handlers_use_the_persisted_policy_without_defaulting_authenticated_keys --offline -- --nocapture
结果：1 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml legacy_handlers_use_auth_snapshot_and_fail_closed_when_snapshot_is_missing --offline -- --nocapture
结果：1 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml chat_completions_counts_one_authenticated_request_without_duplicate_increment --offline -- --nocapture
结果：1 passed，0 failed
```

未运行全量测试；上述构建与聚焦测试均通过。仅有工作区已有 unused/dead-code 与 Windows linker 警告。

## 未处理事项

- 按用户边界未处理 Core request permit、视频幂等原子性、视频 permit 回收/回收语义。
- 未运行全量 Rust suite。
- reservation 若进程重启导致外部 lease 句柄丢失，仍按既有 no-op 结算/释放语义，不扩展跨进程恢复或对账。

## A2.2a 复核修复：管理端保存保护运行态 reservation

- `api_keys::save_from_admin` 在 `KEYS_LOCK` 内合并管理端 Key 配置；已有 Key 保留当前运行中的 `token_reservations`，新 Key 不接受前端带入的 reservation，避免整表保存清掉正在执行的请求额度。
- `api_keys_save` 改用该原子 helper；新增 `admin_save_preserves_runtime_reservations_and_key_list_changes` 回归测试，覆盖配置更新、增删 Key、鉴权开关和恶意/陈旧 reservation payload。
- D 盘 focused 验证：`admin_save_preserves_runtime_reservations_and_key_list_changes` 1 passed；`token_reservation_` 6 passed；认证快照、quota error、Chat 单次计数测试各 1 passed。
- 仍未处理 Core request permit、视频幂等原子性、视频 permit 回收/恢复语义；quota canonical code 继续使用 `quota_exceeded`，并保留 `legacy_code=daily_quota_exceeded`。

---

# Task 3 修复轮次 5：管理端保存不覆盖运行态 Token reservation

日期：2026-09-20
范围：仅修复管理端 `api_keys_save` 的锁内整表保存合并；未扩展到 Core、video 或 quota error 字段。

## 本轮修复

1. `src-tauri/src/api_server/api_keys.rs` 新增 `save_from_admin`：在 `KEYS_LOCK` 内 load 当前文件，按 Key id 应用前端配置；已有 Key 保留当前运行中的 `token_reservations`，新 Key 丢弃前端带入的 reservation，避免前端快照覆盖运行态。
2. `src-tauri/src/commands/api_server.rs` 的 `api_keys_save` 改为只调用该 helper，不再执行无锁 `load` + `save`。`auth_disabled` 的 `Some` 值按调用方传入值生效，`None` 在同一锁内保留当前值。
3. 新增 Rust 回归测试，覆盖已有 reservation 保留、前端 reservation 不生效、Key 新增/删除不变以及 `auth_disabled` 显式更新。

## RED / GREEN 与 focused 测试

测试环境统一使用 `CARGO_HOME=C:\Users\StarLink\.cargo`、`CARGO_TARGET_DIR=D:\gpt\aiwork-key-limits-cargo-target`、stable toolchain PATH 和 `--offline`；没有把 Cargo/test 产物写入 C 盘。

```text
cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml admin_save_preserves_runtime_reservations_and_key_list_changes --offline -- --nocapture
RED：失败于缺少待实现的 `save_from_admin`；实现后 GREEN：1 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml token_reservation_ --offline -- --nocapture
结果：6 passed，0 failed

cargo test --quiet --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_keys --offline -- --nocapture
结果：23 passed，1 failed；失败为已有 A2.2a 测试 `daily_stats_capped_at_90`（实际 `d18`，期望 `d30`），与本轮管理端保存修复无关，未改动统计排序逻辑。
```

`cargo fmt --check` 未能执行：已安装的 stable toolchain 缺少 `rustfmt` 组件；未安装组件或修改文件。`git diff --check` 未发现空白错误。

## 未处理事项

- 保留 `daily_stats_capped_at_90` 的既有失败，不扩大到本轮无关的 A2.2a 统计排序逻辑。
- 未运行全量 Rust suite；完整 suite 的既有失败和其它未提交改动继续按前述报告处理。
- 未修改 Core/video、quota error 字段或任何其它用户改动；提交时仅暂存本轮修复 hunks 与本报告追加内容。

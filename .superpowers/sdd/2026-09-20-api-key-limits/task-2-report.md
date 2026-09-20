# Task 2 实现报告：全局默认值与 Key 级请求限流器

## 状态

实现已完成，当前无编译或接口阻塞。提交号以任务回复中的最终 Git HEAD 为准。

## 修改文件

简报指定文件：

- `src-tauri/src/api_server/gateway_settings.rs`
- `src-tauri/src/api_server/limits.rs`
- `src-tauri/src/api_server/mod.rs`
- `src-tauri/src/commands/api_server.rs`
- `src/types.ts`
- `src/lib/tauri.ts`

为保持新增字段和新 `acquire` 签名下现有 API server 测试/调用点可编译，另外只加入了必要的兼容性改动：

- `src-tauri/src/api_server/assets.rs`：已有设置测试字面量补充 `limit_defaults`。
- `src-tauri/src/api_server/routes.rs`：已有素材/视频调用改用 `acquire_legacy`，已有设置测试字面量补充 `limit_defaults`。

上述两个文件中的其他用户/前序任务改动未加入本提交。

## 接口决定

- `LimitConfig` 增加 `max_video_jobs`，持久化字段均做安全范围规范化；现有四个环境变量继续拥有高于持久化值的运行时优先级。全局值为 0 时规范化为最小有效值；Task 1 的 daily 计数器仍按约定使用 0 表示不限。
- `KeyLimits::effective(global)` 对 nullable Key 字段执行继承，并与全局值取小，Key 不能扩大网关上限。
- `RateLimiter` 内部使用 `Arc`；请求许可和视频任务许可均为 RAII。请求/视频计数只保存 Key ID 和计数，不保存 Key 明文。视频任务许可在 `Permit` 被 drop 前持续占用独立的视频任务额度。
- `acquire` 按生效 Key 限制执行素材上传、素材字节和视频提交滑动窗口；窗口拒绝会回收已申请的请求并发许可。旧调用点暂由 `acquire_legacy` 使用默认 Key 限制适配，后续认证路由可通过 `ApiSharedState` 的 Key-aware helpers 传入 `ResolvedKey.limits`。
- `GatewaySettings` 的 `limit_defaults` 使用 serde 默认值；读取/保存路径返回规范化值，启动 API server 时使用加载后的设置，因此环境变量覆盖会进入 limiter 配置。TypeScript 的 `GatewaySettings.limit_defaults` 设为 optional，以兼容旧版设置页面提交，而 Rust 返回值始终带有规范化字段。
- 简报给出的双过滤 Cargo 命令在当前 Cargo 版本会报 `unexpected argument 'gateway_settings'`；按单过滤器分别执行等价焦点测试。

## RED / GREEN 记录

测试目标使用 `CARGO_TARGET_DIR=D:\gpt\traework-target`，输出均写入 `D:\gpt`。

### RED

命令：

```powershell
cargo test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml limits gateway_settings -- --nocapture
```

结果：Cargo 在编译前拒绝第二个过滤器，报 `unexpected argument 'gateway_settings'`。随后按有效的单过滤器命令运行 RED：

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml limits -- --nocapture
```

结果：退出码 101；输出 `D:\gpt\task2-red-limits-verified.txt` 显示预期的缺失接口/字段，包括 `LimitConfig.max_video_jobs`、`GatewaySettings.limit_defaults`、`KeyLimits::effective`、`acquire_request`、四参数 `acquire` 和 `acquire_video_job`。

### GREEN

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\traework-target'
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml limits -- --nocapture
```

结果：退出码 0，`10 passed; 0 failed`；输出 `D:\gpt\task2-final-limits.txt`。

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml gateway_settings -- --nocapture
```

结果：退出码 0，`8 passed; 0 failed`；输出 `D:\gpt\task2-final-gateway.txt`。

```powershell
& 'C:\Users\StarLink\.cargo\bin\cargo.exe' test --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_server -- --nocapture
```

结果：退出码 0，`369 passed; 0 failed; 107 filtered out`；输出 `D:\gpt\task2-final-api-server.txt`。

补充检查：暂存差异和工作区差异均通过 `git diff --check`。`cargo fmt --check` 未能运行，因为当前 Rust toolchain 未安装 `rustfmt`；这不是编译或接口阻塞。

## API server 测试结果

API server 单元测试全量通过：369 通过、0 失败、107 个过滤。限流器焦点测试 10 通过，网关设置焦点测试 8 通过。

## 未解决问题 / concerns

- 本任务完成了 limiter 的 Key-aware 接口和 `ApiSharedState` 转发入口；现有素材/视频路由仍通过 `acquire_legacy` 兼容旧调用，认证路由把 `ResolvedKey.limits` 接入每条业务路径以及将视频 permit 绑定到具体任务终态，仍应由后续路由/视频任务集成任务完成。
- `cargo fmt` 受环境缺少 `rustfmt` 影响未执行；已有编译警告仍存在，但不影响测试通过。
- 报告要求的测试输出没有写入 C 盘，均位于 `D:\gpt`。

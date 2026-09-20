# Task 3 实现报告：路由能力检查、每日 Token 额度与视频任务生命周期

## 状态与范围

- 基线：`02e80064c4448e3df9ba04aee71333b2af923397`。
- 需求来源：`task-3-brief.md`。
- 未执行 `reset`、`checkout` 或 `clean`；工作区中已有的用户改动和前序任务改动均保留。
- 前一代理留下的 `routes.rs`、`video.rs`、`usage.rs`、`api_keys.rs` 改动已逐项复核：保留有效的统一模型/原生视频资源处理，并补齐缺失的能力校验、Key policy、Token quota 和 permit 生命周期接线。
- 为完成生产接线额外修改了 `auth.rs`、`mod.rs`、`video_worker.rs`；`trae_resource_upload.rs` 是保留的视频实现所依赖的新增模块，也随任务相关变更保留。其余工作区改动未纳入本任务提交。

## 实现覆盖

### 能力开关与普通路由

- chat、video、assets 能力关闭时，在计数、usage 统计和 permit 获取前返回 `403 capability_not_allowed`。
- 认证层和业务路由均覆盖能力检查；公共素材内容路径保持公开读取语义。
- 认证后的 legacy 路由按 Key ID 读取 `ResolvedKey.limits` 快照，并使用该 policy 获取请求/资产/视频提交许可；业务路由不再用 `acquire_legacy` 绕过 Key limits。
- Core chat 保持 Core 限流路径，不叠加 legacy Key quota。

### 每日 Token quota

- 按 UTC 日期保存每日 prompt/completion token 统计，跨日使用新日期记录。
- 已知 usage 通过现有 token 提取和 usage 记录路径入账，并在 Key 锁内完成 quota 检查与更新；没有伪造 token 数量。
- 达到每日 quota 后返回 `429 quota_exceeded`，拒绝请求前不增加请求计数。

### 视频 job permit

- native video 在输入校验成功后取得 `video-job` permit，并把 owned permit 保留到任务终态。
- Core video 在接受新任务后保留 permit；幂等重放和提交失败会释放临时 permit。
- `completed`、`failed`、`canceled`、`timeout` 以及恢复时发现的终态都会释放 permit；未知/未决状态继续持有，直到后续终态更新。
- worker 的上游终态、任务更新和恢复路径都接入释放逻辑；校验失败不会创建/持有 job permit。

### Core 素材

- Core 素材 limiter 的索引改为 `principal.key_id`。
- Core 素材仍使用 Core 资产限流路径，不额外扣 legacy Key 的 Core quota，避免双扣。

## RED 阶段

使用复用的目标目录和离线依赖：

```powershell
$env:CARGO_TARGET_DIR='D:\gpt\aiwork-key-limits-cargo-target'
$env:CARGO_HOME='C:\Users\StarLink\.cargo'
$env:PATH='C:\Users\StarLink\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;'+$env:PATH
$env:AIWORK_ASSET_DIR=$null
```

需求简报中的多过滤器示例按原样尝试后，Cargo 返回 `error: unexpected argument 'video' found`；这是无效命令格式，不是源码阻塞。随后按要求拆成 `routes`、`video`、`usage` 三次单过滤器运行。

RED 编译暴露并随后修复的首批源码错误包括：缺少 `verify_and_consume_for_capability`、缺少 `KeyCheck::CapabilityNotAllowed`、缺少 `retain_job_permit`/`release_job_permit`，以及视频启动函数缺少 owned permit 接线和一个 request 所有权借用错误。一次增量编译还出现过可复现后的临时 rustc incremental ICE；后续复用同一目标目录编译和全量测试均成功，未构成源码阻塞。

## GREEN 验证

每条命令均单独运行，使用 `--offline`，编译目标和测试输出位于 `D:\gpt` 下的目标目录：

```powershell
cargo.exe test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml routes -- --nocapture
# test result: ok. 51 passed; 0 failed; 0 ignored; 0 measured; 430 filtered out

cargo.exe test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml video -- --nocapture
# test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 447 filtered out

cargo.exe test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml usage -- --nocapture
# test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 461 filtered out

cargo.exe test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml api_keys -- --nocapture
# test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 465 filtered out
```

完整 Rust 测试：

```powershell
$env:AIWORK_ASSET_DIR=$null
cargo.exe test --offline --manifest-path E:\AIWORK\workspace\TraeWorkAssistant\src-tauri\Cargo.toml
# test result: ok. 477 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out
```

没有发现既有 Rust 测试失败。`git diff --check` 通过。格式检查未执行成功的原因是当前 stable toolchain 未安装 `cargo-fmt.exe`/`rustfmt.exe`，属于本机工具缺失而非源码错误。编译中仅剩既有 unused/dead-code 及 Windows linker warning。

## 结论与 concerns

- Task 3 的 focused 和完整 Rust 测试均通过。
- 生产接线覆盖认证层、legacy 路由、Core 素材、Core/native 视频提交、worker 终态和恢复路径。
- 未打印或写入报告任何 API Key 明文。
- 唯一已知验证限制是本机缺少 rustfmt；不影响 Cargo 编译或测试结果。

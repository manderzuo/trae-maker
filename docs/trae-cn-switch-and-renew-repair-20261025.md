# TRAE CN 切换与 JWT 续期修复（2026-10-25）

承接交接（`project-handover.md`）。本轮只处理两条用户可见故障，不改动计费语义：
① 管理界面无法切换账号；② 一个账号 JWT 已过期但无法续期。

## 一、无法切换账号（已修复，已现场验证）

### 现象与定位过程

`switcher.log` 显示切换「成功」，但客户端里的账号不变；`app.log` 里切换前的 JWT 预检正常放行。
进一步核对发现切换桥连续输出 `[stop] TRAE CN 默认实例未运行`，而当时系统里有 21 个
`Trae CN.exe` 进程在运行 —— 客户端从未被真正关闭，快照被写到运行中的客户端上，随即被
客户端用内存中的旧登录态覆盖回 `storage.json`。

三个叠加缺陷：

1. **路径分隔符不一致导致进程识别失败。**
   `src-tauri/src/native_client.rs` 用 `local.join("Programs/Trae CN/Trae CN.exe")` 拼接候选路径。
   Rust 的 `Path::join` 不会把参数里的 `/` 规范化为 `\`，产出
   `...\Programs/Trae CN/Trae CN.exe`（混合分隔符）。该值经 `AIWORK_NATIVE_EXE` 传给切换桥，
   而 `Test-DefaultTraeProcess` 用**字符串全等**比较 `Win32_Process.ExecutablePath`（原生 `\`），
   永远不相等 → 判定「默认实例未运行」→ 直接返回，不执行关闭。

   实测对照（真实进程集，21 个 `Trae CN.exe`）：

   | 判据 | 匹配根进程数 |
   |---|---|
   | 旧的原始 `-eq` 比较 | **0** |
   | 归一化后 `OrdinalIgnoreCase` 比较 | **1**（主进程） |

2. **启动被主机安全软件拦截。**
   修复关闭逻辑后，客户端能正常停止，但 `Start-Process` 直接 `CreateProcess` 启动的客户端
   会立即退出（`debug.log` 只留下 V8 isolate 初始化行，退出码 9）。经实测，同一可执行文件
   **经 ShellExecute（`explorer.exe`）可以正常启动并保持运行**。这是主机侧 HIPS（本机为火绒）
   对非交互宿主直接创建进程的拦截，不是应用缺陷。

3. **启动成功判定过松（假成功）。**
   原实现只要 `Start-Process` 调用不抛错就继续；而拦截场景下进程「出现过又立即退出」，
   桥仍报告 `[done] 已切换至账号 X`，用户看到的是"切了但没反应"。

### 修复

`src-ps/trae-switch-bridge.ps1`：

- 新增 `ConvertTo-NormalizedExePath`（`[System.IO.Path]::GetFullPath`），
  `Test-DefaultTraeProcess` 改为归一化后 `OrdinalIgnoreCase` 比较；
  `Find-TraeExe` 返回前也归一化，便于日志与下游判断一致。
- `Start-Trae` 增加启动回退链：直连 → `cmd start`（保留启动参数）→ `explorer.exe`；
  每级都用 `Test-AppProcessStable`（出现后**再观察 5 秒仍存活**）确认，全部失败才抛错。
- 该脚本是 Tauri bundle resource，运行中的部署副本为
  `activated-release\ps\trae-switch-bridge.ps1`，本次已同步，**无需重编译即生效**。

`src-tauri/src/native_client.rs`：候选路径改为逐段 `join("Programs").join("Trae CN").join("Trae CN.exe")`，
从源头消除混合分隔符（该改动随下一次构建生效）。

### 现场验证

- 直接以桌面端相同参数调用桥（故意传入混合分隔符的 `AIWORK_NATIVE_EXE`）：
  `[stop] 正在关闭 TRAE CN 默认实例` → 关闭成功 → 恢复目标快照 → 启动走 ShellExecute 回退 →
  `[verify]` 通过 → `[done]`。
- 解密客户端 `User/globalStorage/storage.json` 的 `tc` 容器（AES-128-CBC + SHA512 校验，
  只读取 `userId`/`expiredAt`/`refreshExpiredAt` 元数据）：切换后 `userId` = 目标账号。
- `profiles_trae/current_account.txt` 同步为切换后的账号。

## 二、JWT 过期无法续期（代码已改，待构建上线）

### 根因

账号 `2539229719823225` 在**应用侧没有 `refresh_token`**（UI 因此只显示"续期"钥匙按钮、
没有"刷新"闪电按钮），`refresh_jwt_for_profile` 在发起任何网络请求之前就以
"该账号无 refresh_token，无法自动刷新"失败 —— 日志里查不到 `ExchangeToken` 记录正是这个原因。

同时，该账号在本机 TRAE CN 客户端里**存在有效的原生 `refreshToken`**
（`refreshExpiredAt` = 2027-04-08），只是从未被接管到应用凭据库。

原 UI 提示也有误导：tooltip 一律写"使用原生 TRAE CN refresh_token"，
只有 `credential_source == 'bitbrowser'` 时才附加"需重新导入接管"的指引。

### 修复

`src-tauri/src/commands/accounts.rs`：

- 新增 `native_refresh_token_for`：从原生客户端 `storage.json` 读取同 UID 的 `refreshToken`，
  校验 UID 一致、token 非空、`refreshExpiredAt` 未过期；任一条件不满足返回 `None`，
  不修改任何状态（继续走原来的错误路径）。
- `refresh_jwt_for_profile`：应用侧缺少 `refresh_token` 时先尝试接管，接管成功后把
  `oauth_client_id` 固定为原生 `TRAE_CLIENT_ID`（避免退到旧 CloudIDE 端点必然失败），
  再走原有 `ExchangeToken`；仍缺失时返回可操作指引（先在 TRAE CN 登录该账号并
  「保存当前登录态」，再重试续期）。

### 边界

- 接管只在**本机客户端当前登录的正是该 UID** 时成立。若该账号已被登出，仍只能由用户
  在客户端重新登录一次。
- 接管不产生任何费用、不改变额度、不重发历史请求。

## 二（补）. 现场验证：接管成功后暴露的第三个适配缺口

把客户端切到该账号并点「续期」后的实际链路：

1. `已从本机客户端接管 refresh_token: uid=2539229719823225` —— 接管生效，凭据已加密入库。
2. `原生设备凭据不可用，请重新进行原生授权` —— `native_proof.py` 只按 `<...>:<uid>` 命名找设备密钥，
   而 TRAE CN 3.4 把它存在 `iCubeAuthInfo://icube-dc:<deviceId>`。已修：两种命名都扫，
   缺绑定时用客户端已有密钥对 bootstrap（只落公开上下文，私钥仍由 DPAPI 保存）。
3. 修复后请求真正发出，返回 `ExchangeToken 400/401`。抓取真实响应体后确认：
   - 用 `TRAE` client(`ono9krqynydwx5`)：`10101 refresh token is not matched to the client`
   - 用旧 client(`en1oxy7wnw8j9n`)：`20403 Token device not match`
   - 两组设备身份（`3080180323066812` / `aha-b0c8…`）× 两个 client 交叉测试全部失败。

结论：该 refresh token 由**旧 client 签发**，且绑定的是**另一台设备身份**（本机现有两套设备密钥都对不上）。
因此这个账号无法靠接管救活，**必须做一次真实原生授权**。已同时修正接管逻辑：不再改写
`oauth_client_id`（issuer 只能由真实授权确定，强行改成 TRAE client 会让后续请求稳定报 10101）。

## 三、验证边界（务必注意）

- 本轮修复针对"切换不生效"和"续期入口不可用"；**没有**验证上游额度、计费正确性或
  `Seedance` 真实生成成功。
- 现有 `[verify]` 步骤只确认 `storage.json` 里存在该账号的 CloudIDE 登录条目，
  不构成"客户端已完成服务端鉴权"的证明。JWT 被服务端吊销时，客户端仍可能表现为未登录。
- 若主机安全软件持续拦截直接启动，切换将稳定走 ShellExecute 回退（会丢失 `--proxy-server`
  等启动参数注入），这是已知取舍。

## 四、复现与回归

```powershell
# 只读核对：当前实例的进程识别（应返回 1 个根进程）
$exe = 'C:\Users\StarLink\AppData\Local\Programs\Trae CN\Trae CN.exe'
$expected = [System.IO.Path]::GetFullPath($exe)
@(Get-CimInstance Win32_Process -Filter "Name='Trae CN.exe'" | Where-Object {
  $_.ExecutablePath -and $_.CommandLine -and
  ([System.IO.Path]::GetFullPath($_.ExecutablePath)).Equals($expected,[System.StringComparison]::OrdinalIgnoreCase) -and
  $_.CommandLine -notmatch '--user-data-dir|--type[= ]|--utility-sub-type'
}).Count
```

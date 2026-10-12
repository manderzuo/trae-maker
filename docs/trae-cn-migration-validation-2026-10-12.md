# TRAE CN 迁移交付与验证记录

日期：2026-10-12。客户端：本机统一版 TRAE CN 3.4.2。本记录保留迁移阶段的检查，该阶段运行修复代码为 `3c9e86e`（包含文字修复 `cda3d62`），EXE SHA256 为 `2f9841c12c6e670162f53de9bb3634b7bb999585df124f903bda73e66dbad286`。后续单纯交接文档更新不改变该阶段二进制。2026-10-12 晚间又部署了视频读取恢复补丁，当前运行 SHA256 为 `d4f37f1c35446b1b48223a48f3b12e72a3d6b1411db30402b731948679fd4697`；Core 也已更新。当前状态以[视频读取恢复记录](video-stream-recovery-20261012.md)为准。

## 迁移阶段交付状态

- 文字协议代码已与桌面身份分离，GLM-5.3-Flash 真实辅助调用及费用确认通过，原 4001 失败任务未自动重放。
- 视频协议恢复 0.1.50 / 20260811，原能力配置与证据字节保持。普通生成、尾帧参考、完整视频参考三条 5 秒 / 480p / 16:9 公网请求完成、下载、全流解码并结算；129 项专项回归通过。
- 实际桥接账本为 Schema9。升级前后 17 张原表 2827 行一致，21 笔历史待核费用逐笔保持；新积分包正常授权，两种静止恢复模式均零联网、零收费派发。
- 运行入口仍为既有 E 盘启动器，后端修复已激活；Studio Web 和云端 Core/Studio 发布包未因本次视频修复替换。
- 用户已追加授权推送源码与更新交接资料，迁移分支及仓库 `main` 同步此次修复；没有发布新安装器或 GitHub Release。

完整请求、实扣和运行证据见[视频迁移修复与验收](video-migration-repair-20261012.md)。以下首轮记录为历史阶段，不能用其中的旧 EXE 或未实测范围代替当前交付结论。

## 交付范围

| 链路 | 实现与验证范围 |
| --- | --- |
| 注册与原生 OAuth | 沿用现有注册入口；按所选客户端生成 PKCE、client_id、设备公钥和请求身份，AuthCode 交换与发起请求绑定同一客户端。编译及隔离 fixture 已通过，未真实新建账号或完成一次交互式授权。 |
| JWT 与续期 | 保留旧 refresh token 的原始 issuer；新原生协议使用 P-256 设备签名，验证返回 UID 后保存完整凭据。用户信息读取已按新版 `x-cloudide-token` 和 `Result` 格式实测；完整上游 refresh token 轮换未实测。 |
| 导入导出与快照 | issuer 与 DPAPI 加密设备绑定随账号保存；同一 Windows 用户下的绑定迁移已通过 fixture。旧槽只复制到缺失的新槽，保留原槽；本机 7 个新槽解密及 UID 一致性校验通过。 |
| 每日签到 | GUI、无界面模式共用续期和签到规则；本机编译后的 `--daily-checkin` 执行完成，7 个已签到、0 个失败，无新的签到领取。 |
| 切号、保存与重启 | 目标客户端贯穿续期、快照及桥接；默认实例按 EXE、用户目录、进程树关闭，独立实例保留，重启保持代理/CDP 参数。隔离检查通过，真实保存当前客户端登录态通过；未实际跨账号切换。 |
| 项目接力与多实例 | CDP 校验所属客户端及默认实例，选择会话页并跳过悬浮按钮页；真实只读 `get_chat_session`、`get_messages` 均返回 code 0，后者返回 2 条消息。未真实创建分享、自动接力或操作独立实例。 |
| 代理与网关 | 原生捕获验证 UID、凭据时序和 issuer，再保存设备上下文；代理与网关使用统一客户端身份。外部 API 与 `solo_work_lite` 协议值保留，本机健康、鉴权状态和模型列表读取通过。 |

私钥在助手目录中使用当前 Windows 用户的 DPAPI 保护，在客户端快照中使用 tc 加密容器。跨机器或跨 Windows 用户导入需要重新授权。缺失已绑定签名材料时保留旧凭据并提示授权，不生成替代私钥继续刷新。

## 执行过的检查

- Rust 聚焦检查：`native_` 37/37、`daily_checkin::tests` 4/4、`unified_oauth_binds` 1/1、`commands::bitbrowser::tests` 8/8；这些筛选可能有重叠，不合计为独立测试数。
- Python 客户端/代理检查 5/5，设备签名与导入导出检查 3/3，既有 `auto_checkin` 检查通过。设备签名检查验证精确请求字节、错误公钥拒绝、同用户迁移及私钥不以明文落盘；未用其他 Windows 用户执行 DPAPI 解密测试。
- PowerShell 桥接脚本解析 0 错误；默认/独立进程区分及启动参数检查通过。
- 前端 TypeScript/Vite 构建通过，`DiscoverModal` 聚焦检查 1/1。
- `cargo build --manifest-path src-tauri/Cargo.toml --release --offline --locked` 通过；复用现有 Python 3.12.10 运行时及依赖，同步 13 个业务脚本。未下载工具链或引入新依赖。
- 构建仍有 Rust 警告与 Vite 大包提示；未运行全量 Tauri 检查。

## 首轮本机激活结果（后续版本见当前交付状态）

运行文件：`E:\trae-studio\work\aiwork-capacity-qa61\activated-release\ai-work-assistant.exe`。

EXE SHA-256：`b006cc90c4b3b749475454d24b91ac77d8811a655bea7c6cdc407e3b1832556a`。激活 EXE 与 release 构建哈希相同，Python 资源与 PowerShell 桥接脚本逐文件匹配；助手及其代理子进程从该交付目录运行。

原生目标是 `C:\Users\StarLink\AppData\Local\Programs\Trae CN\Trae CN.exe`，数据目录是 `C:\Users\StarLink\AppData\Roaming\Trae CN`。当前登录属于账号池；新旧客户端当前 UID 不同，读取与保存均未混用。

| 核对项 | 结果 |
| --- | --- |
| 账号池 | 7 个账号；迁移前后 UID 集合不变 |
| 新快照 | 7 个有效账号槽，0 个无效可见槽；备份及标记不作为账号展示 |
| 旧快照目录 | 原有 17 个条目保留；该数包含备份与标记，并非账号数 |
| 网关账号选择 | 原有启用配置不变，状态摘要为 5 个启用账号 |
| 网关只读检查 | `/health`、带鉴权 `/status`、`/v1/models` 均 HTTP 200；inflight 0，模型列表 19 项。列表成功不代表所有模型生成已实测 |
| 新版用户信息接口 | HTTP 200，返回 UID 与当前原生登录一致 |
| 原生会话读取 | 两个只读 RPC code 0；未上传消息或发布分享 |
| 无界面签到 | completed；credential_accounts 7，already 7，ok 0，failed 0 |
| 安静恢复 | charged_dispatches 0；保留 unknown 约束及账本守卫 |
| 计划任务 | 桌面启动与每日签到任务均为 Ready，既有任务与启动入口保留；未通过任务调度器重复触发签到 |

原始验证日志和不含凭据的摘要保存在本地 `rollout-backup\trae-cn-20261012\validation-evidence`。账号、JWT、refresh token、私钥和原生登录备份不进入 Git。

## 保留与回滚

分支 `fix/aiwork-capacity-recovery` 与当前工作区保留；源码推送按本次追加授权执行，安装器与版本号保持。原 EXE、Python、PowerShell 资源及助手/原生核心状态备份在 `E:\trae-studio\work\aiwork-capacity-qa61\rollout-backup\trae-cn-20261012`，最终视频运行更新另有 `rollout-backup/video-migration-*` 静止备份。

当前账本已经是 Schema9。需要关闭新增容量时，先确认本助手无在途请求，保留同一 Schema9 兼容二进制和当前账本，以 `--disable-new-package-capacity` 启动。不得直接恢复初次迁移的 Schema8 二进制或用旧数据库覆盖后续费用。Python/PowerShell 资源须与目标二进制匹配，保留新旧快照与账号库；显式恢复仍须保留 unknown、独占租约和静止检查。

## 本次取舍与后续项

1. 按迁移计划的禁止全量 Tauri 检查约束执行聚焦检查；代价是未覆盖其他模块的全部回归。
2. 针对审查未判断的上游行为，依据已安装 3.4.2 的代码和真实只读接口补充设备签名及新版请求头；注册、交互式 OAuth、完整续期仍未实测。文字辅助和三种视频路径后续已按当前交付记录真实验收，其他视频规格未逐档实测。
3. 针对安装器与计划任务交付，采用本地 EXE 加配套脚本，保留入口和回滚副本；未验证安装器分发，资源异常需要本地回滚。
4. 首次交付只保留本地分支和工作区；用户后续已明确授权推送修复，本次同步远端并保留工作区，不重复索要许可。
5. 删除本次临时工作目录的操作被自动审批策略拒绝，返回原因为 `blocked by policy`；保留 `.superpowers/sdd/2026-10-12-trae-cn-unified-migration`，代价是继续占用少量本地磁盘空间，不影响运行版本。

延后的小项：帮助弹窗仍将 TRAE CN 与 Trae 的快照写成两个应用；旧别名可能解析到同一个统一版槽，文案有歧义，路由已使用实际客户端。

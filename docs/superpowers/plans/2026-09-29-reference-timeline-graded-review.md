# 参考视频时长分级核验：本地修复验收

## 用户确认的范围

采用“正常快速通过、小幅差异结构核验、较大差异实际解码”的分级方案。一帧是快速路径的分界，不是拒绝阈值。预占使用核验后较大的可信时长，最终扣费仍使用上游真实回执。先完成本地修复，激活程序前确认运行中的任务已暂停；本记录不表示已部署或已推送 Git。

## 根因与实现

旧 `bridge_reference` 在视频 `mdhd` 时长与 `stts` 样本总时长不一致时立即拒绝。此前生成的正常 H.264/B-frame 文件：72 帧、24fps，`mdhd=37376`、`stts=36864`、时间基准 12288；差 512 ticks，恰好一帧。它可解码为 3 秒，旧路径却返回 `reference_video_metadata_invalid`。

现在先生成 Timeline，保留文件头、样本表、编辑表及经核验的合成时间线中较大的时长：

- 正常一致的元数据仍走原快速路径，保留已支持的单位速率编辑表与音频 priming/padding。
- 出现视频时长差异时，核对 `stsz/stsc/stsd/stco/co64` 的样本数、大小、chunk 映射和所有数据范围，确保它们实际位于本文件 `mdat` 内且不重叠；核对 `ctts` 数量、偏移与合成时长。
- 差异不超过该视频最大单帧时长时，结构核验通过即可兼容，不要求部署新客户端或重编码原文件。
- 更大的差异使用本机 `video-frame-extractor.json` 中 SHA256 校验通过的 FFmpeg 完整解码视频。分别核对 stdout 机器进度与 stderr 逐帧时间线，要求帧数、时间基准和样本总时长一致；保留更大的解码呈现时长。不会只信 `format.duration` 或进程返回成功。

核验仅针对 `read_owned` 已验证所有权、大小和摘要的原始字节；不接受用户提供的本机路径或远程地址。解码使用随机私有快照，原素材保持不变，无网络协议、无 shell、禁止外部 data references；与尾帧提取共用 2 个进程、8 个等待者的资源限制，单进程 1 解码线程、最大 8388608 像素、30 秒运行超时，stdout 64KiB / stderr 4MiB 上限。正常、失败和超时结束后删除快照并释放槽位。

缺少配置、摘要不匹配、启动失败、超时或输出超限返回可重试的核验不可用；现有 prepare 路由将它们作为 `503 budget_preparation_failed`，不是确定性素材无效 400。确认损坏或无法建立一致时间线才返回 `reference_video_metadata_invalid`。未增加自动二次提交或伪造零扣费回执。

## 验证证据

先加入真实 MP4 回归测试：旧代码在差一帧和差多帧的可用视频上返回 metadata_invalid，观察到失败后才修改生产路径。

修复后验证：

- 此前失败的 16098 字节 H.264 文件保留为无账号信息的回归样本，核验时长 3042ms。
- 1 秒真实视频的文件头改为 1.125 秒：无解码器配置也能经结构核验通过，按 2 秒参考时长计入风险配置。
- 同一视频的文件头改为 1.5 秒和 0.5 秒：完整解码后分别按 2 秒和 1 秒计入参考风险配置，原字节不变。
- 错误帧数、错误 chunk 映射、越界数据、不可解码的视频数据、超过 60 秒的保守时长仍拒绝。
- 多参考视频时长相加后向上取整，跨 Key 素材不可读取。
- 解码器摘要不匹配不能绕过验证；连续 3 次人为超时后仍可重新核验成功，临时目录无快照残留。
- 媒体日志不能独立伪造解码成功，必须核对机器进度和所有逐帧计数。

后端完整命令：清除当前 shell 继承的 `AIWORK_*` 运行目录环境变量后，使用既有 `E:/AIWORK/workspace/TraeWorkAssistant/src-tauri/target`，运行 `cargo test --manifest-path src-tauri/Cargo.toml --locked --offline -- --test-threads=1`：**777 passed / 0 failed / 8 ignored**。定向参考时长测试 **11 passed / 1 ignored**，深度核验测试 **2 passed**。

首次默认并行运行：724 passed / 53 failed / 8 ignored；失败集中于账务测试争用其刻意共享的测试专用主机排他锁。未改动线上互斥锁，按其串行运行要求复测后全部通过。完整失败列表保留在 `.superpowers/reference-timing-full-tests.log`，串行记录在 `.superpowers/reference-timing-full-serial-tests.log`。编译有 25 条既存警告，不声称零警告。

`npm test`：**50 passed / 12 files**。`npm run build`：通过，存在既存的大 bundle 提醒。

Release 构建通过：`cargo build --release --manifest-path src-tauri/Cargo.toml --locked --offline`，用时 1 分 31 秒，77 条编译警告。候选程序放在 `E:/AIWORK/candidates/20260929-reference-timeline-graded/ai-work-assistant.exe`，大小 33466368 字节，SHA256 `b1f60aa7f591f199300a28b105708ab6854900866be1ca028a9f8977c5e9bb43`。未改启动器；运行 PID 65920 仍使用 `20260929-reference-video-natural-spec` 版本。

## 保留边界与激活条件

这不是对所有媒体格式的无限制放行：仍要求非碎片化 MP4、单视频轨、已支持的样本表及单位速率编辑表，素材上限 32MiB、参考保守总时长上限 60 秒；复杂重定时和不能核验的规格继续拒绝。深度路径依赖本机受信解码器及其支持的 codec / showinfo 功能。

没有消耗上游积分、没有修改 Key 余额、没有清除历史未决账目、没有重启正在运行的助手、没有改公网 Core、没有提交或推送 Git。此次复现的临时 2674 字节探针已清理；16KB 回归样本为有意保留的测试输入。

第一次定向测试继承了运行目录环境变量，产生一个 160 字节的合成测试素材。随后测试均清除运行目录变量进行隔离；该单个遗留文件在核对合成样本的时间基准/帧数/字节数且确认不在运行素材索引中后已删除，未清理其他账号素材。

## 2026-09-29 追加：用户授权部署及 Git 推送

上文“未部署/未推送”为本地修复结束时的历史状态。用户随后明确要求“部署，然后git推送”，本次按以下顺序执行。

- 再次运行完整后端串行回归：777 passed / 0 failed / 8 ignored；前端 50 passed。
- 公网 Core 只读预检确认 active requests / operations / steps 均为空；独立核实的旧失败残留、五条历史 unknown 请求及 13 条桥接未决预算原样保留。
- 将已核验候选程序和未变更的运行资源发布到 `E:/AIWORK/releases/20260929-reference-timeline-graded`，保留旧版本和旧启动器备份。优雅关闭原 PID 65920，未强制终止；停止后备份 `bridge-billing.before.sqlite3`，quick_check=ok。
- `E:/AIWORK/start-aiwork.ps1` 已指向新版本。运行 PID 62560，API 7864 与代理 8899 均监听；实运行文件 SHA256 与候选程序一致，为 `b1f60aa7f591f199300a28b105708ab6854900866be1ca028a9f8977c5e9bb43`。
- 公网桥接读取确认 charge_ready=true、recovery_required=false、fenced_accounts=0，未决预算仍为 13；未手动确认 recovery、未释放历史未知款项。
- 经公网 HTTPS 用普通 Core Key 核验模型目录 HTTP 200；图片/视频上传 HTTP 200、重复上传保持同一资产，互换类型 HTTP 400；真实尾帧提取 HTTP 200，PNG 120 字节，摘要与 Key 绑定一致，未授权调用 HTTP 403。非法生成规格继续正确拒绝。
- 验收前后预算步骤数量不变，未提交新视频、未消费上游积分。公网 Core 本次不换包，仍使用已部署的 `20260929-helper-json-terminal-cleanup`。

Git 按组件归属分开处理：助手代码至 `manderzuo/trae-maker`，Core 已部署源码至 `manderzuo/Trae-core`。临时检查脚本、日志、运行时数据库、账号凭据及发布包均不提交。

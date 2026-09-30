# 本地 AI Work 尾帧提取

尾帧接口处理已经完成的任务，不提交视频，不新增预算。需要专用桥接凭据及精确 request/budget/Key/account/execution/result 绑定。普通客户端无需安装提帧程序。

在 AI Work 数据目录放置 `video-frame-extractor.json`：

```json
{"path":"E:/AIWORK/tools/ffmpeg/9.0.2/ffmpeg.exe","sha256":"3256173f3f8bffd7df12227c68adf68025edb1832273a9530688a7bb1ed8edec"}
```

仅允许本地绝对路径；每次使用验证程序摘要。升级必须选择明确版本、校验发布方安装包摘要并同步版本锁，不能遇到不匹配就接受当前文件的新摘要。不能用任意 PATH 程序、其他软件附带程序或客户端参数代替。程序必须支持本地 MP4 解码、PNG 编码、showinfo 和 image2。不重生成原视频，不影响普通生成、下载和积分结算。

独立工具固定为 GyanD FFmpeg 9.0.2 essentials Windows build，来源由 [FFmpeg 官方下载页](https://ffmpeg.org/download.html)列出；安装包 SHA256 为 `60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba`，见[发布方摘要](https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip.sha256)。版本、来源、安装包和程序摘要保存在 `build-assets/ffmpeg-release.json`。保留随包 LICENSE、README 和源代码版本链接；GPLv3 构建如再次分发须履行相应源代码与许可义务。二进制不进入 Git，不附加自动升级器。

安装（只准备，不启用）：

```powershell
./scripts/install-video-frame-extractor.ps1 `
  -ArchivePath E:/AIWORK/tools/ffmpeg/ffmpeg-9.0.2-essentials_build.zip `
  -ToolsDirectory E:/AIWORK/tools/ffmpeg
```

脚本离线校验固定安装包和程序摘要，只提取自己的 FFmpeg、许可证和 README，不复制或修改其他应用文件。不覆盖已有不同版本文件；产生 `9.0.2/video-frame-extractor.pending.json`，不改生产配置、不重启服务。部署需要一并保留独立工具目录，不能只复制助手 exe。测试可用 `SEEDANCE_TEST_FFMPEG` 指定该固定构建的另一绝对路径，仍必须符合版本锁摘要。

历史更正：2026-09-29 的本机配置借用了 SteelSeries 附带程序；这不是 AI Work 安装的程序，也不是独立工具部署。旧程序摘要与当前文件不一致，不足以证明是哪次更新造成。本次改为专用工具，不以关闭校验或改写为外部程序当前摘要来修复。

旧灰度曾提取864×496、timestamp5000ms的PNG，来源MP4摘要与实际客户端下载文件一致；不能把旧灰度当作本次独立工具的公网验收。本次本地合成夹具已验证不同 MP4 独立提帧、同素材缓存复用，以及工具配置错误的真实 HTTP 路由。Core 传输必须保留长度及所有来源校验头；仅假传输测试不足以验证此契约。

工具预检为鉴权 `GET /internal/bridge/v2/frame-extractor/health`，只返回 `ready` 或固定错误码，不返回路径和命令，不新增预算。明确选择上传视频尾帧的请求在辅助模型付费前预检，实际提帧时再校验。工具配置缺失、文件不可用、程序摘要不符分别返回 `frame_extractor_unconfigured`、`frame_extractor_unavailable`、`frame_extractor_digest_mismatch`，贯穿 Bridge、Core 和失败原因持久记录。

这三类已确认工具错误允许 Core 将无未结束付费步骤的请求明确收尾，释放执行并发；辅助模型账单仍按真实回执处理，不伪造零扣费。网络中断、资源繁忙和无法确认结果不能按工具错误收尾。修好工具不会复活已失败请求；需要生成时用户发起新请求。旧版卡住请求在切换待启用配置前必须通过配套新协议确认收尾，避免后台自动继续派发旧视频；不能单独替换生产配置。

非正常重启若出现 `recovery_required`，先检查同机单活动实例，再按[单机恢复流程](bridge-budget-recovery.md)显式确认保留旧未知占用。不要删除数据库或关闭费用核对来恢复派发。

固定参数、不经过 shell，输入协议仅 file/pipe；只解码末尾两秒。进程超时30秒，输出最多8MiB，并发2、等待队列8。程序指纹只验证提帧工具；每个源视频独立计算素材 SHA256，绝不与上一请求素材比较，也不按提示词改变指纹。缓存键由素材摘要、程序摘要及算法版本组成，同内容可复用、不同内容分开。来源和尾帧分别保存 SHA256、像素尺寸和最后可解码帧时间。源文件持有有期限的磁盘租约，崩溃租约过期后可恢复清理。

测试须先从测试进程环境中移除 `AIWORK_VIDEO_DIR`，不得将夹具写入正式视频目录。本机合成1秒/8fps夹具的最后一帧时间为875ms。生产能力是否开放仍取决于独立的续写契约验收，提帧成功不等于原生视频延长已获支持。

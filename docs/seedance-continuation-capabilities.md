# Seedance 续写能力边界

初次取证日期：2026-09-29；实现基线 c8428a8。2026-10-12 已修复统一 TRAE CN 迁移的身份绑定并完成三种视频路径真实验收；下文保留首次灰度历史，当前状态见末节。

本机 TRAE 官方 Seedance 插件 1.0.1 仅声明 `GenerateVideo`。安装清单没有实际工具参数，现有 relay 记录只有会话/项目定位元数据，不能证明首帧或视频延长参数契约。当前原生提交使用 `tool_text_to_video_stream`，普通参考字段为 `image_asset_ids` 等；不能照搬另一供应商的帧角色。

- `tail_reference`：普通参考图生成一个新片段。需要尾帧提取、归属校验和真实生成通过后，由部署者安装证据文件；默认关闭。
- `native_video_extend`：将所属完整视频映射为 `video_asset_ids`，生成独立新片段。需要明确参数映射、来源与子视频摘要、不同请求身份和已结算视频费用证据；默认关闭。
- `native_first_frame`：没有已验证的严格首帧锁定映射，始终关闭。普通生成和参考图不受该开关影响。

只读 `GET /internal/bridge/v2/video-capabilities` 使用现有专用桥接 Key 鉴权，不开放给匿名或普通 Core Key。

数据目录的 `video-capabilities.json` 记录 provider、client_version、plugin_version、contract_version、evidence_digest。证据固定读取同目录 `video-continuation-evidence.json`，不接受任意路径。两文件各限制64KiB。

支持 `tail-reference-v1` 和 `video-reference-continuation-v1`。配置必须绑定 `trae_native`、`video_protocol::CLIENT_VERSION`（0.1.50）和插件1.0.1；该版本是编译视频适配器身份，不是桌面客户端3.4.2。生成、上传、资源查询和预检使用同一适配器0.1.50 / 20260811，登录/续期/权益继续使用真实桌面身份。

尾帧证据包含已验收日期、`verified_modes`中的`tail_reference`、`parameter_mapping.tail_reference="image_asset_ids"`及`output_semantics="new_segment"`。视频参考证据另需`native_video_extend`模式、`video_asset_ids`映射、来源/子请求与成片摘要、`financial_state="settled"`及正的视频实扣。配置摘要为原证据字节的SHA256。缺失、损坏、适配器版本不匹配或摘要改变时保持未验证；不能改写旧证据版本来开放未知能力。

这些文件由受信任部署者管理；摘要用于检测版本/证据变化，不是视觉衔接效果的自动证明。

## 2026-09-29 真实灰度证据

本机已运行4133b37发布包；更新后的真实MCP通过公网Core完成V1参考图→V2改夜景→C1尾帧新片段，三个视频均完成、真实结算并保存系统Downloads。V2 request_07yvXM9id6TcWhyFo9JxWg 的尾帧SHA256为 `6c24aa165ae16e83e7a55352bc3f639301f493ec94f1c2ae0a017059df0b70d6`；C1 request_vxaGh0i2QQhr6CFE3XiVpg 的受保护发送体确有原图和同摘要尾帧两张参考，视频实际56.208积分，debt0。C1产物805103字节、摘要 `a115deaa6fe689c8b4e83b8bf27b8cdaf3dce65bf88c2d749a865792e676b50f`。

已将真实来源、任务/预算/版本、下载摘要与实扣写入本机数据目录证据文件，摘要 `35c5a8faa44b8634d284399c1e8482c84bf876df52f0eaa6f10a56f604761d26`。配置绑定编译适配器IDE_VERSION0.1.50（不是客户端界面版本）、插件1.0.1。真实桥接查询返回tail_reference=true、两种native=false；公网周Key的临时测试例外已移除，其他Key仍旧路径。

范围仅5秒480p16:9、一组参考/改版/尾帧样本，以及实际MCP和API历史标记只读恢复。TRAE/DSH原生界面新功能未重新验收；尾帧参考不自动拼接，不保证首帧严格一致。证据必须随适配器升级重新核验，不能复制摘要开启未知版本。

## 2026-10-12 迁移后的当前状态

恢复既有`video-reference-continuation-v1`证据与0.1.50适配器绑定，保留配置和证据原始字节。公网能力为`tail_reference=true`、`native_video_extend=true`、`native_first_frame=false`。

普通生成、尾帧参考和完整视频参考各一次5秒/480p/16:9请求均生成成功、下载并完整解码、真实结算；两个子请求使用同一已完成基础版本，分别带1张尾帧图和1段完整视频。真实样本不代表其他规格已逐档校准，也不代表原片自动拼接或严格首帧锁定。请求与费用详情见[视频迁移修复与验收](video-migration-repair-20261012.md)。

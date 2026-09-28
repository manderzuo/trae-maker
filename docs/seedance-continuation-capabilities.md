# Seedance 续写能力边界

取证日期：2026-09-29；实现基线 c8428a8；并非付费生成验收报告。

本机 TRAE 官方 Seedance 插件 1.0.1 仅声明 `GenerateVideo`。安装清单没有实际工具参数，现有 relay 记录只有会话/项目定位元数据，不能证明首帧或视频延长参数契约。当前原生提交使用 `tool_text_to_video_stream`，普通参考字段为 `image_asset_ids` 等；不能照搬另一供应商的帧角色。

- `tail_reference`：普通参考图生成一个新片段。需要尾帧提取、归属校验和真实生成通过后，由部署者安装证据文件；默认关闭。
- `native_first_frame`、`native_video_extend`：`not_verified`，无实现映射，始终关闭。普通生成和参考图不受这些开关影响。

只读 `GET /internal/bridge/v2/video-capabilities` 使用现有专用桥接 Key 鉴权，不开放给匿名或普通 Core Key。

数据目录的 `video-capabilities.json` 记录 provider、client_version、plugin_version、contract_version、evidence_digest。证据固定读取同目录 `video-continuation-evidence.json`，不接受任意路径。两文件各限制64KiB。

当前唯一已实现可声明契约为 `tail-reference-v1`。配置必须绑定 `trae_native`、编译时 IDE_VERSION 和插件1.0.1。证据必须包含已验收日期、`verified_modes:["tail_reference"]`、`parameter_mapping:{"tail_reference":"image_asset_ids"}`、`output_semantics:"new_segment"`；配置摘要为证据原始字节的SHA256。缺失、损坏、版本不匹配或证据摘要改变都返回全部false。

这些文件由受信任部署者管理；摘要用于检测版本/证据变化，不是视觉衔接效果的自动证明。本轮尚未安装证据文件、未启用任何续写能力。

# 视频迁移修复与验收

目标：恢复统一 TRAE CN 登录后的既有 Seedance 提交、素材上传和续写，同时保留全部历史费用、未知占用和幂等身份。沿用用户的修复、真实测试与部署授权；不修改 Studio Web 界面。

## 已定位的迁移缺口

1. 桌面登录版本 3.4.2 / 20250325 覆盖了已验收视频适配器的 0.1.50 / 20260811 身份，续写证据也错误地与桌面版本比较。原验收文档明确 0.1.50 指编译适配器，而非桌面界面版本。
2. 五个启用账号的原生权益查询 HTTP 200，新增未使用奖励包合计每账号 3900–4200 积分，均有稳定字符串 entitlement_id。旧 R 占用阻止整账号静止重基线，使独立的新包也不能计入执行容量。旧 R 21 笔不具备可释放证据。

## 修复约束

- 以 video_protocol 统一视频生成、资源解析、素材授权和费用预检的原视频适配器身份。登录、续期、权益读取继续使用真实桌面身份。能力配置与证据原始字节不改，严格首帧锁定继续为 false。
- schema9 单独记录新 UNUSED 包见证和当前读取水位。本金、包 ID、产品与期限不可变；不替换旧容量快照、anchor，也不改 P/R/D 和费用回执。
- 只允许已明确配置为专用的账号；必须来自受信任原生查询，已知产品 208/209/221、完整期限、明确未使用状态和稳定字符串 ID。与旧 anchor 的数值 ID 别名不得重复计入。
- 每次准备确认同包仍存在、有效、元数据一致、用量明确。当前确认最多有效 60 秒，并在准备、占用、consume 和 send-intent 检查。未知、撤销、缺失、时钟回退和旧响应晚到均不能授权；专用配置关闭立即生效。
- 当前合法 anchor 已吸收同包时排除额外本金，避免双计。原有 D 扣减、P/R 保护、实时余额预检和单次发送 CAS 保留。
- schema8→9 原子升级，失败完整回滚，最新布局只校验不补洞。升级后恢复入口兼容 schema9；启动参数 `--disable-new-package-capacity` 关闭新增容量计算，结果和账单查询继续可用。产生新事实后不得用 schema8 数据库备份覆盖账本。

## 交付检查

- [x] 行为红灯：桌面升级误关已验收视频能力；独立新包不能在保留 R/D 时授权。
- [x] 专项回归：协议头、证据绑定、新包重复/别名/过期/撤销/未知、读取排序、anchor 吸收、发送前再次拒绝。
- [x] schema9 失败迁移与完整布局校验，原记录保持。
- [x] 独立审查、离线 locked release、静止备份和更新，运行健康与旧未知费用保持。
- [x] 公网一次最小视频生成，以及尾帧和视频参考续写，结果与真实结算分开核验。

设计审查已提出并纳入：原生查询开始时间排序、当前包确认、ID 类型歧义、全部发送边界、即时专用授权撤销和 schema9 兼容恢复。实际测试结果与请求编号将在完成后追加。

静止专项回归共 129 项通过，2 项明确标记的 live/ignored 测试未运行：grants 9、capacity source 14、budget 5、prepared 8、billing 53、runtime 10、background balance 7、capabilities 5、upload identity 1、planner 17。两个主要行为回归先观察到失败再修复。独立复审无剩余 Critical/Important。

第一次激活发现维护入口把额外禁用开关计入旧参数个数，导致恢复 CLI 退出；应急启动保留新版 schema9 二进制并关闭新增容量，未恢复旧数据库。比对原 schema8 静止备份与实际 schema9：17 张原表共 2827 行完全一致，旧 R 21 笔保留。参数解析已修正，最终激活已实际验证两种恢复模式。

最终 release SHA256 `2f9841c12c6e670162f53de9bb3634b7bb999585df124f903bda73e66dbad286`，2026-10-12 17:53 激活 PID 29980。两种静止恢复均成功：禁用新增容量为 false、正常为 true；均 network_calls=0、charged_dispatches=0、credentials_read=false，retained_unresolved_budgets=21。最终二进制的 129 项专项回归再次通过，更新前后 17 张原表 2827 行完全一致；health 200，配置文件摘要不变。

公网内部能力恢复为 tail_reference=true / native_video_extend=true / native_first_frame=false，contract_version=video-reference-continuation-v1。未改动能力配置与原证据；摘要分别为 `10747601b8d533a225c22d39695c973f0efd7884e3ecaa542a7538e46b551554`、`37b507bbf7f9160875e07cdf7c2753085616ec17d91ac3ed1e82129036a8d7e7`。

## 2026-10-12 公网真实验收

沿用用户的真实测试授权，通过 Studio 相同的公网 Core 和已配置 Key 提交。每个诊断先保存独立固定幂等身份与 send-intent 文件，再且仅再发一次 POST；完成后只读查询和取成片。没有重放原失败任务，也未重提上一轮已结束的容量诊断。

输入为灰色桌面上的白球，无人物、文字或音乐；三例均请求 5 秒 / 480p / 16:9。两条续写从同一个已完成、同 Key 所属的基础版本分支生成独立新片段，未替换原片或自动拼接。

| 路径 | Core 父请求 | 视频实扣积分 | GLM-5.3-Flash 辅助实扣积分 |
| --- | --- | ---: | ---: |
| 普通生成 | request_VY88cmjpv_EFc5ekrCnwQg | 56.208 | 0.0432 |
| 尾帧参考 | request_UYP1rLpqPZ5Sc_OfrhABYw | 56.208 | 0.3176 |
| 完整视频参考 | request_-284ipRMZi1ZWR--W3ZNhA | 66.5664 | 0.1932 |

六个步骤均 execution_state=succeeded、financial_state=settled、dispatch_attempted=1；三份 work version 均 completed，两个子版本 parent_version_id 与基础版本一致。视频合计 178.9824，辅助合计 0.554，本次三例总实扣 **179.5364 积分**。397 积分是风险占用，不能误报为实际扣费。

本地受保护发送体只在内存解密核对，不输出提示词或凭证。实际参考数量分别是 0 图/0 视频、1 图/0 视频、0 图/1 视频；三条视频预算最终均为 D，与 Core 的预算 ID 和实扣一致。

成片均由同 Key 的公网 content 接口返回 HTTP 200。服务端没有独立 ffprobe，所以使用已经配置的 Windows FFmpeg 9.0.2 完整解码；三份均成功解码 121 帧，H.264、24fps、864×496，容器时长约 5.09 秒，视频帧时长约 5.04 秒。请求档位为 480p/16:9，上游返回的实际像素尺寸按上述记录，不声称精确 480 像素或严格首帧锁定。

| 成片 | 字节数 | SHA256 |
| --- | ---: | --- |
| 普通生成 | 654394 | ca42895483363b0960cbc1ba393e214db0310f37ed2947c95829311a9f7dde36 |
| 尾帧参考 | 729925 | e472d512801e785d7ee23a019446d37829958e91e13a09738f6c13b5956cbab9 |
| 完整视频参考 | 881599 | 660d586e3b7d2fc6062f8e0a0eac188f82221a4c441280d2e2ffd402f5cd6a85 |

18:04 最终运行检查：inflight=0、unfinishedExecutions=0、capacity P=0、quick_check=ok，PID 29980 运行的文件与最终 release 摘要一致，未启用紧急禁用参数。更新前的 21 笔 R 全行逐笔比对仍一致。原 4001 失败父请求 request_2b9rq9OniwECm_VLhF-3gw 仍 failed，仍只有原失败辅助步骤与未核实费用，没有新增视频步骤；旧容量诊断 request_HwdNj4lC6ZG8wYPywrYDCg 仍 failed，未重放。

私有 EXE、账本备份、测试日志、成片和受保护运行材料全部保留在忽略的 rollout-backup/ 或 Studio work/video-failure-4001-20261012/，不进入 Git。Studio Web、云端 Core/Studio 发布包、登录凭证和原能力证据均未改。此次真实样本覆盖三种视频路径，其他时长/清晰度/多素材组合没有逐档实测，原风险政策和校验继续生效。

# 2026-10-12 修复源码与交接同步

## 晚间追加修复

视频响应读取补丁代码位于 AI Work `28d792d` 与 Core `50a32dc`，Studio `e886d5e` 记录已有未知状态契约。三仓库默认分支和对应修复/交接分支继续以正常快进同步，当前运行网关及 Core 已部署；Studio 应用代码保持原生产版本。78 项回归与原故障任务只读验收见[最新修复记录](video-stream-recovery-20261012.md)。

以下是早前迁移源码同步的历史记录，运行版本与验证结论以最新修复记录为准。


用户在完成视频迁移修复后，明确要求将修复推送仓库并更新相关位置。本次同步源码、用户文档、更新日志、运维/恢复说明和跨仓库交接；不发布新安装器，不增加收费测试，不改写运行账本。

| 仓库 | 默认分支 | 修复/交接代码位置 |
| --- | --- | --- |
| [trae-maker](https://github.com/manderzuo/trae-maker) | main | 迁移及文字/视频修复随fix/aiwork-capacity-recovery同步；视频运行代码3c9e86e、文字cda3d62，后续提交仅更新资料与Git忽略规则 |
| [Trae-core](https://github.com/manderzuo/Trae-core) | main | 4313a11，原已部署的无视频终态状态修复、两项回归及跨仓库交接 |
| [video-maker](https://github.com/manderzuo/video-maker) | feat/aiwork-studio | f097470，包含当前生产源码分支62f4d75及原默认分支6734c3c的F0部署资料；合并仅增加三份既有发布文档，新增交接不改应用源码 |

各仓库默认分支保留已有历史，通过正常快进推送更新，不使用force。修复/发布分支同样保留；远端最终SHA以本次推送后读取确认。Studio发布工作区为`E:/trae-studio/work/studio-migration-publish-20261012`，原开发工作区的未提交改动保持，未被混入发布。

已同步README、CHANGELOG未发布安装器条目、用户手册迁移补充、迁移说明/验证、Seedance能力边界、Schema9专用容量与恢复要求、AGENT契约和Studio恢复点/操作单。

现行AI Work EXE与视频验收保持[修复记录](video-migration-repair-20261012.md)的SHA256，21笔历史待核费用不变；源码推送不替换该运行版本。Core和Studio保留现有云端发布包。Schema9后不得用Schema8账本或不兼容二进制回退；紧急关闭新增容量使用同一兼容程序的`--disable-new-package-capacity`。

本次发布前检查包括Core无视频终态两项隔离回归、相关文档链接/格式、暂存范围、私有材料排除、Studio合并前后应用源码一致性及远端ref确认。129项桥接回归与三条已付费视频验收在上轮完成，运行代码未改变，不为源码同步重复发起收费请求。私有账本、凭据、运行EXE、诊断与测试成片不进入任何仓库。

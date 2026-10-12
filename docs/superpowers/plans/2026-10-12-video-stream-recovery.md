# 视频流等待与恢复 Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan in this session; request an independent final code review.

**Goal:** 视频流读取故障或超过总时限后停止本地等待，保留真实请求及未知费用，并向 Studio 返回清楚的未知状态。

**Architecture:** 单独提取视频 SSE 消费器；ureq 请求整体 deadline 覆盖阻塞网络读取。闭合诊断持久化到原预算身份，Core 只读状态接口透传未知；重启通过已有显式恢复入口保留 P/R/D，不重发旧任务。

**Tech Stack:** Rust / ureq 2.12.1 / SQLite / existing authenticated bridge v2.

**Spec:** 本计划内的 Goal、Architecture 与 Global Constraints 即本轮已授权的修复规格。

## Global Constraints

- 单个视频请求最长等待 13 分钟，先于 Core 14 分钟观察期限结束；不改变文字、上传、资源查询请求。
- 读取、协议、超时错误是 unknown，不能证明未收费；不释放 P、不生成零费用回执。
- 原 task/session/request/budget 身份不变；原故障任务不能自动重新提交。
- 不读取或发布原始提示词、token、账号凭据；只输出闭合错误代码。
- 使用已安装工具链，不下载依赖；真实宿主互斥测试前停止本机助手。

## Review Focus

- 持续返回同一读取错误的 reader：第一次错误即停止。
- 连续心跳、没有换行、无数据连接：真实网络 deadline 均能打断。
- 最后结果没有 done、JSON 损坏、多行事件：不能报成功或无限等待。
- 重启后失去本地 worker：保留原任务、预算和账单，仅添加未知诊断。
- 迟到真实结果：未知诊断不覆盖权威终态，不改变收费规则。

### Task 1: bounded video stream

**Files:** src-tauri/src/api_server/video_stream.rs; video.rs; mod.rs; bridge_dispatch.rs; bridge_runtime.rs.
**Interfaces:** consume(reader, callback) returns a closed Fault; Fault::diagnostic() returns video_stream/outcome_known=false.

- [x] 添加有限错误 reader 的回归，先在抽出的原循环上确认失败。
- [x] 显式处理读取错误、事件长度上限与 EOF；视频 POST 设置 780 秒整体 deadline。
- [x] 记录同一任务/预算诊断；旧 generation 的遗留视频只添加 interrupted 未知诊断。
- [x] 执行聚焦流、视频、诊断、恢复与单次提交回归。

### Task 2: public disposition and activation

**Files:** Core budget_flow.rs, budget_errors.rs, seedance_feedback.rs; related tests; deployment notes.
**Interfaces:** 原 GET /v1/videos/<request> 对已记录未知执行返回 task.status=unknown 和安全说明。

- [x] 添加未知视频状态回归并修复 Core 透传；核对 Studio 对 unknown 的现有支持。
- [x] 构建与独立审查；备份本机账本/任务索引，仅停止被确认空转的旧实例。
- [x] 更新运行二进制，以既有受保护 recovery API 恢复；检查所有金额/身份保留及无自动提交。
- [x] 发布 Core 补丁并只读查询同一故障请求；更新文档与已授权仓库分支。

Ruling: 延续用户已有修复、推送与部署授权，在同一会话执行并保留完整私密备份；不再次要求许可。已补恢复游标与 stale DTO 一致性，以独立审查和65条回归验证。源码提交与仓库同步结果记录于修复文档对应仓库历史。

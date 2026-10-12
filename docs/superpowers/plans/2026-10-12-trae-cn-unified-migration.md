# TRAE CN Unified Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将已有注册、授权、签到、切号和网关链路迁到统一 TRAE CN，保留旧凭据与快照。

**Architecture:** Rust 集中解析客户端安装、数据目录、OAuth 和请求身份。所有桌面链路消费相同结果；Python 与 PowerShell 接收解析结果并保留独立运行时的兼容检测。

**Tech Stack:** Windows、Rust/Tauri、React/TypeScript、Python、PowerShell；复用既有依赖。

**Spec:** docs/superpowers/specs/2026-10-12-trae-cn-unified-migration-design.md

**Execution status:** 四项任务已完成源码适配、本地构建与激活；实际检查结果和端到端验证限制见 [交付验证记录](../../trae-cn-migration-validation-2026-10-12.md)。以下复选项保留计划原文。

## Global Constraints

- 不自动升版本号、推送、合并或发布 GitHub Release。
- 旧账号、refresh token 归属、快照和运行资源必须可恢复。
- 不改 solo_work_lite、计费 fail-closed、Core host lease 与 unknown 恢复约束。
- 验证清除 AIWORK_* 环境，使用隔离 fixture 与已有 E 盘工具链，不下载工具链、不运行全量 Tauri 测试。
- 保留现有 Tauri 命令和外部 API；新 DTO 字段 serde default。

## Review Focus

- 新旧目录 UID 不同：当前登录只读取选中客户端。
- 存量 refresh token：不能升级为新 issuer；跨账号结果拒绝。
- 同 UID 快照冲突与迁移中断：不覆盖新槽，不公布半成品。
- 自动接力与多实例：路径、快照、进程和 CDP 属于同一客户端。
- 部署与计划任务：脚本资源和无界面模式使用相同身份，保持账本守卫。

### Task 1: 原生客户端解析与路由

**Files:** 新增 src-tauri/src/native_client.rs；修改 main.rs、commands/env.rs、process.rs、profile.rs、switch.rs、trae_apps.rs、relay.rs、instances.rs、native_snapshot.rs、src-ps/trae-switch-bridge.ps1。

**Interfaces:** 产生 NativeClientProfile（target_app、exe、data_dir、profiles_name、app_version、base_version、oauth_client_id、auth_from），resolve_for(target, settings)、resolve_default()、canonical_target(target)。消费者不得再次硬编码旧路径。

- [ ] 写隔离安装 fixture：仅旧版、统一版、共存、有效自定义路径、失效路径、pre-3.4 IDE、无安装；断言 target/data/snapshot/OAuth 相互一致。
- [ ] 运行聚焦 native_client Rust 测试；Expected: RED。
- [ ] 实现解析和所有读写/启动消费者，桥接脚本保留旧 TargetApp 兼容。
- [ ] 同一测试与编译检查；Expected: GREEN。
- [ ] 提交明确文件，核对 HEAD 与分支引用。

### Task 2: 凭据归属与快照迁移

**Files:** models.rs、commands/oauth.rs、accounts.rs、bitbrowser.rs、trae_apps.rs、native_snapshot.rs；新增聚焦测试。

**Interfaces:** 消费 Task 1 profile；产生 RawAccount.oauth_client_id 可选字段、原生授权请求 client_id、UID 刷新守卫与 migrate_legacy_profiles(data_root)。

- [ ] 写新授权 client_id、旧/未知刷新 issuer、跨账号返回拒绝、快照冲突/无效/临时目录/中断测试。
- [ ] 运行聚焦测试；Expected: RED。
- [ ] 授权交换绑定请求 client_id；导入导出/vault 保留 issuer；有效快照原子复制到 profiles_trae，保留 profiles。
- [ ] 运行聚焦测试；Expected: GREEN。
- [ ] 提交明确文件，核对引用。

### Task 3: 签到、代理与网关身份

**Files:** 新增 src-python/native_client.py；修改 auto_checkin.py、device_proxy.py、trae_project_state.py、src-tauri/src/python.rs、commands/accounts.rs、proxy.rs、api_server/mod.rs 及身份参数消费者。

**Interfaces:** 消费 profile 与 oauth_client_id；Python 通过 AIWORK_NATIVE_CLIENT JSON 获取同一身份，独立启动自行检测；模型/媒体 function 和鉴权格式保留。

- [ ] Python fixture 测试统一版头部、旧配置回退、捕获 issuer 和原生目录选择；Expected: RED。
- [ ] 实现共享身份、代理捕获与无界面签到资源加载；自动切号事件携带 target_app。
- [ ] Python 聚焦测试及 Rust 编译检查；Expected: GREEN。
- [ ] 提交明确文件，核对引用。

### Task 4: UI、构建与本地激活

**Files:** src/store.ts、types.ts、lib/tauri.ts、页面/弹窗文案、迁移文档及打包资源。

**Interfaces:** 消费实际目标；兼容旧持久状态，默认 TRAE CN。

- [ ] 验证自动切号/手动切号目标贯穿保存、接力、回滚；必要行为用聚焦前端测试。
- [ ] 修改界面与文档；tsc/vite、聚焦回归、离线 release 构建；Expected: PASS。
- [ ] 一次 fresh-context 全分支审查并修复重要问题，保留验证证据。
- [ ] 备份当前 EXE/资源和真实登录核心文件，迁移快照，停止自身服务后替换并启动本地助手。
- [ ] 核对运行 EXE、资源、客户端解析、账号/快照数量、网关 health/认证只读查询与计划任务。真实授权/签到/生成分别报告已验证程度。
- [ ] 提交源码与最终验证文档，核对引用；不提交凭据与运行目录。

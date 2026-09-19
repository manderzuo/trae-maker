# Phase 4D Core 管理闭环设计

## 目标

在现有 Tauri 管理窗口中接通 Core 的管理员能力：查看 Core 状态、用户、API Key
元数据和用户永久额度；创建用户、签发/撤销 Key、充值额度以及启用/禁用用户。所有
写操作继续由 Core 的事务和 admin Principal 授权，桌面端只保存本次会话中用户输入的
管理员 Key，不把 Key 写入 settings、日志或 Core。

这是一条独立的 Phase 4/5 管理最小闭环。它不改变 legacy `api_keys.json` 的 off/shadow
兼容路径，不把 Core Key 与 legacy Key 合并，也不接入在线支付、真实上游计费或公网管理
接口。

## 当前事实和边界

- `src-core` 已有 `users`、`api_keys`、`quota_ledger`、`quota_reservations` 和审计表；
  `create_user_as_admin`、`issue_api_key_as_admin`、`grant_as_admin` 已由 Tauri command
  使用，但没有完整只读投影和撤销/用户状态管理入口。
- `src-tauri/src/commands/core.rs` 已暴露 `core_status`、用户创建、Key 签发、额度充值；
  `main.rs` 已注册这些 command。前端目前只管理 legacy `api_keys.json`，没有 Core 管理 Tab。
- 管理员认证必须重新使用 Core `authenticate_api_key` 后再调用
  `authorize_admin_principal`；不能接受用户输入的 `admin` 字符串、普通用户 Key 或前端传来的
  user role 作为授权证明。
- Core 返回的 Key 只返回 `prefix`、scope、owner、状态和时间元数据；明文只在新签发的
  command 响应中出现一次。digest、credentials、上游账号和其他用户的请求/素材不进入 UI。

## 管理投影

新增下列 Core 只读模型和方法，所有 `*_as_admin` 方法在查询事务开始时验证 admin Principal：

```text
CoreUserAdminView {
  id, name, role, status, created_at_ms, updated_at_ms
}

CoreApiKeyAdminView {
  id, user_id, name, prefix, scopes, status, created_at_ms, revoked_at_ms
}

CoreStore::list_users_as_admin(principal) -> Vec<CoreUserAdminView>
CoreStore::list_api_keys_as_admin(principal, user_id: Option<&str>) -> Vec<CoreApiKeyAdminView>
CoreStore::quota_balance_as_admin(principal, user_id, resource_kind) -> QuotaBalance
CoreStore::set_user_status_as_admin(principal, user_id, active: bool) -> CoreUserAdminView
CoreStore::revoke_api_key_as_admin(principal, key_id) -> ()
```

`set_user_status_as_admin` 只能写 `active`/`disabled`，禁止把最后一个 active admin 禁用，
也禁止管理员把自己禁用。禁用用户会让其全部 API Key 在鉴权层失效；历史账本、任务、审计
和额度不会删除。撤销 Key 是幂等的，重复撤销只返回成功且不重复写入有效状态。

## Tauri command 契约

在 `src-tauri/src/commands/core.rs` 增加：

```text
core_users_list(admin_api_key) -> Vec<CoreUserAdminResponse>
core_api_keys_list(admin_api_key, user_id: Option<String>) -> Vec<CoreApiKeyAdminResponse>
core_quota_balance(admin_api_key, user_id, resource_kind) -> CoreQuotaBalanceResponse
core_user_set_status(admin_api_key, user_id, active) -> CoreUserAdminResponse
core_api_key_revoke(admin_api_key, key_id) -> ()
```

这些 command 只接受 admin Key；错误信息向 UI 返回固定的 `admin_api_key_required`、
`admin_api_key_invalid`、`admin_api_key_not_authorized` 或 Core 的非敏感业务错误，不返回
SQL、digest、凭据和其他用户隐私。`main.rs` 必须注册 command，并有源码注册测试。

## 桌面 UI

新增 `src/components/api/CoreAdminPanel.tsx`，作为 `ApiManagerModal` 的 `Core 管理` Tab：

1. 管理员 Key 输入框只保存在 React state；刷新/关闭弹窗后清空，不写 localStorage、Zustand
   持久化状态或日志。
2. 顶部显示 schema、Core mode、数据库路径、运行状态及管理员 scheduler 聚合状态；数据库
   路径只作为本机诊断展示。
3. 用户表显示 owner-safe 的 id/name/role/status/额度，支持创建用户、切换 active/disabled。
4. Key 表显示 owner/name/prefix/scopes/status/时间，支持按用户筛选和撤销；不显示明文或
   digest。签发成功只在页面内显示一次明文，并提供复制按钮和“已保存后关闭”提示。
5. 额度表支持选择用户和 `text`/`video_job`/`image` 资源类型，显示 available/held；充值
   必须输入正整数和不可为空的审计 reason，提交后重新读取余额。
6. 所有异步调用有 loading/error 状态；错误不清空已有列表；禁用/撤销/充值前使用已有
   Modal 二次确认组件，禁止 `window.confirm`。

## 测试和证据

- Core 集成测试：admin 能读投影；普通用户/伪造 admin id 被拒绝；投影不含 digest/plaintext；
  自禁用、禁用最后 admin、未知用户和未知 Key 被拒绝；重复撤销幂等；额度充值写一条可审计
  ledger 并返回正确余额。
- Tauri command 测试：每个 command 通过真实 Core auth 走通，普通 Key 和空 Key 被拒绝，
  `main.rs` 注册完整。
- Frontend：`npm test` 和 `npm run build`；使用 mock `invoke` 验证管理 Tab 不把 Key 写入持久
  store，并验证 loading/error/明文一次性展示。
- Cargo/npm 的临时目录、target 和日志放 `D:\gpt`；Rust 命令使用 `--offline --locked`，
  清空并断言 `AIWORK_*` 环境变量；不访问真实网络或真实账号。

## 回滚

回滚只移除新的 UI/command 和读投影，不删除 Core 表、账本或审计记录；旧的 Core 数据库
仍可由现有 command 和 API 鉴权使用。管理员 Key 遗失时只能撤销并重新签发，不允许从数据库
恢复明文。

## 未解决边界

本设计不宣称已经提供公网管理员 API、不提供在线支付、不改变真实上游积分的单位，也不把
legacy 日限额当作 Core 永久额度。Phase 4D 完成后，Phase 4 的队列公平性/重启对账深化和
Phase 5 的部署、Skill/MCP 说明仍需单独验收。

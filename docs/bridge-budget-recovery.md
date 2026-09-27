# V2 桥接异常恢复（单机）

本接口用于新版本的显式恢复，不是打开视频计费闸门、补积分、退款或重发任务。当前开发代码尚未部署；数据库 schema 6 不支持旧 schema 5 二进制直接打开回滚。

所有接口只接受 AI Work 的桥接管理员 Key，普通 Core 用户 Key 不具备权限。不要把管理员 Key 交给外部客户端。

1. `GET /internal/bridge/v2/recovery` 读取当前 `instance_id`、`generation`、`recovery_required`、`charge_ready`、`retained_unresolved_budgets`、`fenced_accounts`。
2. 只在 `recovery_required=true` 时，由管理员确认运行环境是同一 Windows 主机的单活动实例，且接受保留旧未知任务的占用后，POST 同一路径：

```json
{
  "instance_id": "从本次GET取得的值",
  "generation": "从本次GET取得的值",
  "acknowledge_retained_unknowns": true
}
```

请求必须对应本次恢复态。正常运行、关服中、旧世代或缺少确认均拒绝；不得用循环重试强行打开服务。请求成功只表示当前进程取得新派发准入，仍需已有计费策略、可用账号容量和 Core Key 额度。

## 保留的事实

- 旧运行记录变为 unknown，所有 P/R/D、账单和原token记录保持。旧token不能获得新发送许可，已发送请求不自动重发。
- 未提交的账号重基线事务中止，原余额和已扣金额不变。账单冲突造成的账号隔离仍然保留。
- 结果查询、账单补偿继续按原 request/Key/budget 归属处理。恢复不是旧账已结清的证据。
- 旧世代 Prepared 即使过期也不会自动退款（离线旧备份不证明未收费），但不再挡住当前世代未消费预算的有界过期清理。

跨机器同时运行、跨机器凭据解密、任意旧数据库恢复后认定“从未发出”均不在自动恢复能力内。不能通过删除数据库或清零占用来解决未知状态。

## 容量更新

schema 6 已提供持久静止栅栏与逐预算覆盖审计的内部原语。普通余额读数不带费用覆盖证明；尚无合格生产来源驱动时，不自动清D，也不提供由客户端传入金额和静止布尔值的公网更新接口。

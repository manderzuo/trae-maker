# Seedance 缺档临时预冻结（2026-09-29）

在 `<data_dir>/bridge-budget-policy.json` 的现有 version=1 / profiles 配置旁增加 `video_fallback`。保留原 profiles，不能用本例替换整份配置。本次代码支持此字段，但本文件不代表生产已启用或已部署。

```json
{
  "video_fallback": {
    "enabled": true,
    "model_family": "seedance2-fast",
    "baseline_resolution": "720p",
    "baseline_duration_seconds": 15,
    "hold_microcredits": 397000000,
    "policy_version": "receipt-baseline-20260929-v1",
    "source": "readonly verified final 720p15s receipts; 7-day high-water 360639200 microcredits; buffered 10 percent rounded upward",
    "expires_at_ms": 1791302399000
  }
}
```

基准审计：2026-09-29 只读查询本机账本，最近7天3笔720p/15秒、无参考、成功且final的正数视频回执。解密原报价封存并核对请求、Key、账号及预算身份；最高实际360.6392积分，10%余量后向上取整为397积分。有效期到北京时间2026-10-06 23:59:59。该数字不是所有账号或参考输入的精确价格，更不是上游最高扣费保证；它是本次管理员授权的临时风险额度。测试使用的413积分是独立fixture，不是生产基准。

选择顺序：本账号同规格实扣 > 有效同规格政策/原生估价 > 同模型/分辨率/参考数量、生成与参考时长均能向上覆盖的档 > 显式启用的统一兜底。不再从较短生成档按比例推算较长生成档。重复/错误配置不能被兜底掩盖。

原始生成参数、比例和实测参考秒数不变。397积分已经是最终预冻结额，不再二次加余量，也不冻结Key全部余额。素材、所有权、格式、Key额度及上游账号容量校验不变。

可信成功final回执仍是持久化校准事实：新规格按原规格和账号自动形成下次报价，重启可用；比例不拆价格档。超出预占照实结算并进入校准，不截断；重复、冲突和未知账单不会产生重复扣费或有效新价格。关闭/过期/无效兜底仍明确拒绝缺档请求，不绕过财务保护。

发布步骤：完成两仓回归及自审；检查本机/公网没有真实在途工作；备份原配置，只合并上述字段；替换通过构建验证的助手与Core；使用新唯一请求做真实参考视频验收，记录首次hold、最终实扣、额度释放与下一次学习结果。旧失败请求不重放、不补猜历史原因。

## 2026-09-30生产启用记录

用户授权推送及部署后，源码cf034fc已推送至trae-maker的main与修复分支；公网Core源码21304db在独立Trae-core仓库发布。助手仍在本机运行，不部署到公网。

生产路径为 `E:/AIWORK/releases/20260930-fallback-risk-first-cause/ai-work-assistant.exe`，运行SHA256核实为 `86b676a45481fed84c590d833dda8c45c02692e4cba09dacbfab53d1c2268e66`。Python依赖、当前业务脚本和PowerShell资源已随发布目录补齐；API7864和代理8899均已监听。公网Core路径和SHA256也已独立核对。

生产策略已仅合并上面的video_fallback，原29个profiles保持一致；397积分兜底有效期仍为北京时间2026-10-06 23:59:59。旧启动器、策略和桥接账本备份位于 `E:/AIWORK/backups/20260930-fallback-risk-first-cause`。

重启触发的桥接保护经过正式认证恢复流程接管：先确认无运行执行、无待提交rebase，保留13个未决预算，随后确认charge-ready。部署前后17张桥接表中仅代际元数据变化，财务、预算、容量、Key及250条预算回执保持一致，未直接写库绕过保护。

公网health/admin及认证models查询通过；旧失败任务仍明确提示历史原因缺失，未补猜旧拒绝原因。本轮未提交新的付费视频，不能把发布验证视为新的真实素材生成、实扣结算及本地下载验收。停用监控没有恢复。

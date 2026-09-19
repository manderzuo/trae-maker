import { useCallback, useEffect, useState } from 'react';
import {
  Copy,
  Database,
  ListChecks,
  KeyRound,
  LockKeyhole,
  Plus,
  RefreshCw,
  ShieldCheck,
  UserRound,
} from 'lucide-react';
import { Badge, Modal, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type {
  CoreApiKeyAdminView,
  CoreIssuedApiKeyResponse,
  CoreQuotaBalanceResponse,
  CoreStatus,
  CoreUserAdminView,
  CoreVideoJobAdminView,
} from '../../types';

type CoreAdminAction =
  | { kind: 'user'; user: CoreUserAdminView; active: boolean }
  | { kind: 'key'; key: CoreApiKeyAdminView }
  | { kind: 'grant'; userId: string; resourceKind: string; amount: number; reason: string };

export function normalizeCoreScopes(value: string): string[] {
  return [...new Set(value.split(',').map((scope) => scope.trim()).filter(Boolean))];
}

export function coreJobTone(state: string): 'green' | 'amber' | 'slate' | 'red' | 'blue' {
  if (state === 'succeeded' || state === 'canceled') return 'green';
  if (state === 'unknown' || state === 'cancel_requested') return 'amber';
  if (state === 'failed') return 'red';
  if (state === 'running') return 'blue';
  return 'slate';
}

function errorMessage(error: unknown): string {
  return String(error).replace(/^Error:\s*/, '').slice(0, 180);
}

function formatTime(ms: number | null): string {
  if (!ms) return '—';
  return new Date(ms).toLocaleString();
}

function statusTone(status: string): 'green' | 'amber' | 'slate' | 'red' {
  if (status === 'active') return 'green';
  if (status === 'disabled') return 'amber';
  if (status === 'revoked') return 'red';
  return 'slate';
}

export default function CoreAdminPanel({
  onSubModalChange,
}: {
  onSubModalChange?: (open: boolean) => void;
}) {
  const toast = useAppStore((state) => state.pushToast);
  const [adminApiKey, setAdminApiKey] = useState('');
  const [status, setStatus] = useState<CoreStatus | null>(null);
  const [scheduler, setScheduler] = useState<{
    accounts: number;
    enabled_accounts: number;
    fresh_observations: number;
    stale_observations: number;
    active_leases: number;
    unknown_leases: number;
    slot_saturated: number;
    reader_failures: number;
  } | null>(null);
  const [users, setUsers] = useState<CoreUserAdminView[]>([]);
  const [keys, setKeys] = useState<CoreApiKeyAdminView[]>([]);
  const [videoJobs, setVideoJobs] = useState<CoreVideoJobAdminView[]>([]);
  const [jobStateFilter, setJobStateFilter] = useState('');
  const [selectedUserId, setSelectedUserId] = useState('');
  const [keyFilterUserId, setKeyFilterUserId] = useState('');
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [balance, setBalance] = useState<CoreQuotaBalanceResponse | null>(null);
  const [resourceKind, setResourceKind] = useState('video');
  const [confirmAction, setConfirmAction] = useState<CoreAdminAction | null>(null);
  const [issuedKey, setIssuedKey] = useState<CoreIssuedApiKeyResponse | null>(null);

  const [newUserId, setNewUserId] = useState('');
  const [newUserName, setNewUserName] = useState('');
  const [newUserRole, setNewUserRole] = useState('user');
  const [newKeyName, setNewKeyName] = useState('');
  const [newKeyScopes, setNewKeyScopes] = useState('');
  const [grantAmount, setGrantAmount] = useState('');
  const [grantReason, setGrantReason] = useState('');

  const hasAdminKey = adminApiKey.trim().length > 0;

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await api.core.status());
    } catch (error) {
      setStatus(null);
      setError(`读取 Core 状态失败：${errorMessage(error)}`);
    }
  }, []);

  const loadLists = useCallback(async (key = adminApiKey) => {
    const normalized = key.trim();
    if (!normalized) {
      setError('请输入 Core 管理员 API Key');
      return;
    }
    setLoading(true);
    setError('');
    try {
      const [nextUsers, nextKeys] = await Promise.all([
        api.core.usersList(normalized),
        api.core.apiKeysList(normalized, keyFilterUserId || null),
      ]);
      const [nextScheduler, nextVideoJobs] = await Promise.all([
        api.core.schedulerStatus(normalized),
        api.core.videoJobsList(normalized, jobStateFilter || null, 100),
      ]);
      setUsers(nextUsers);
      setKeys(nextKeys);
      setScheduler(nextScheduler);
      setVideoJobs(nextVideoJobs);
      if (!selectedUserId && nextUsers.length > 0) setSelectedUserId(nextUsers[0].id);
      toast('success', `Core 管理数据已刷新（${nextUsers.length} 个用户 / ${nextKeys.length} 个 Key）`);
    } catch (error) {
      setError(errorMessage(error));
      toast('error', `Core 管理数据加载失败：${errorMessage(error)}`);
    } finally {
      setLoading(false);
    }
  }, [adminApiKey, jobStateFilter, keyFilterUserId, selectedUserId, toast]);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus]);

  useEffect(() => {
    onSubModalChange?.(confirmAction != null || issuedKey != null);
  }, [confirmAction, issuedKey, onSubModalChange]);

  useEffect(() => {
    if (!hasAdminKey) return;
    const timer = window.setTimeout(() => {
      void loadLists(adminApiKey);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [keyFilterUserId, jobStateFilter]); // 过滤器改变后刷新，不把输入框每次按键都提交到后端

  const runMutation = async (action: () => Promise<unknown>, successMessage: string) => {
    setBusy(true);
    try {
      await action();
      toast('success', successMessage);
      await loadLists();
    } catch (error) {
      toast('error', errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  const createUser = async () => {
    const id = newUserId.trim();
    const name = newUserName.trim();
    if (!id || !name) {
      toast('error', '请填写用户 ID 和名称');
      return;
    }
    await runMutation(
      () => api.core.userCreate(adminApiKey.trim(), id, name, newUserRole),
      `用户「${name}」已创建`,
    );
    setNewUserId('');
    setNewUserName('');
  };

  const issueKey = async () => {
    const userId = selectedUserId.trim();
    const name = newKeyName.trim();
    if (!userId || !name) {
      toast('error', '请选择用户并填写 Key 名称');
      return;
    }
    setBusy(true);
    try {
      const result = await api.core.apiKeyIssue(
        adminApiKey.trim(),
        userId,
        name,
        normalizeCoreScopes(newKeyScopes),
      );
      setIssuedKey(result);
      setNewKeyName('');
      setNewKeyScopes('');
      await loadLists();
    } catch (error) {
      toast('error', `签发 Core Key 失败：${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  };

  const grantQuota = () => {
    const amount = Number.parseInt(grantAmount, 10);
    const kind = resourceKind.trim();
    const reason = grantReason.trim();
    if (!selectedUserId || !kind || !Number.isFinite(amount) || amount <= 0 || !reason) {
      toast('error', '请填写用户、资源类型、正整数额度和原因');
      return;
    }
    setConfirmAction({ kind: 'grant', userId: selectedUserId, resourceKind: kind, amount, reason });
  };

  const readBalance = async (userId = selectedUserId, kind = resourceKind) => {
    if (!userId.trim() || !kind.trim() || !hasAdminKey) return;
    try {
      setBalance(await api.core.quotaBalance(adminApiKey.trim(), userId.trim(), kind.trim()));
    } catch (error) {
      toast('error', `读取额度失败：${errorMessage(error)}`);
    }
  };

  const confirmMutation = async () => {
    if (!confirmAction) return;
    const action = confirmAction;
    setConfirmAction(null);
    if (action.kind === 'user') {
      await runMutation(
        () => api.core.userSetStatus(adminApiKey.trim(), action.user.id, action.active),
        `用户「${action.user.name}」已${action.active ? '启用' : '禁用'}`,
      );
    } else if (action.kind === 'key') {
      await runMutation(
        () => api.core.apiKeyRevoke(adminApiKey.trim(), action.key.id),
        `Key「${action.key.name}」已撤销`,
      );
    } else {
      await runMutation(
        () => api.core.quotaGrant(adminApiKey.trim(), action.userId, action.resourceKind, action.amount, action.reason),
        `已向 ${action.userId} 增加 ${action.amount} 点 ${action.resourceKind} 额度`,
      );
      setGrantAmount('');
      setGrantReason('');
      await readBalance(action.userId, action.resourceKind);
    }
  };

  const copyIssuedKey = async () => {
    if (!issuedKey) return;
    try {
      await navigator.clipboard.writeText(issuedKey.plaintext);
      toast('success', 'Core Key 已复制；明文只在本次签发结果中展示');
    } catch (error) {
      toast('error', `复制失败：${errorMessage(error)}`);
    }
  };

  return (
    <div className="space-y-3">
      <div className="card p-4">
        <div className="mb-3 flex flex-wrap items-center gap-2">
          <ShieldCheck size={17} className="text-brand-500" />
          <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">Core 多用户管理</h3>
          <Badge tone="violet">独立账本</Badge>
          <span className="text-xs text-slate-400 dark:text-zinc-500">
            管理 Core 用户、Key 和资源额度；与旧版本地 API Keys 分开
          </span>
        </div>

        <div className="rounded-lg border border-amber-200 bg-amber-50/70 p-3 text-xs text-amber-800 dark:border-amber-800/50 dark:bg-amber-900/10 dark:text-amber-200">
          管理员 Key 仅保存在当前页面内存中，不会写入配置文件或浏览器存储。Core Key 明文只在签发成功时展示一次，请立即保存。
        </div>

        <div className="mt-3 flex flex-wrap items-end gap-2">
          <div className="min-w-64 flex-1">
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">Core 管理员 API Key</label>
            <input
              className="input font-mono text-xs"
              type="password"
              autoComplete="off"
              placeholder="仅用于本次管理会话"
              value={adminApiKey}
              onChange={(event) => setAdminApiKey(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') void loadLists();
              }}
            />
          </div>
          <button className="btn-outline flex items-center gap-1 !px-3 text-xs" onClick={() => void loadLists()} disabled={loading}>
            {loading ? <Spinner className="h-3.5 w-3.5" /> : <RefreshCw size={14} />}
            加载管理数据
          </button>
          <button className="btn-ghost flex items-center gap-1 !px-3 text-xs" onClick={() => void refreshStatus()}>
            <Database size={14} />
            刷新状态
          </button>
        </div>

        {status && (
          <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-slate-500 dark:text-zinc-400">
            <Badge tone={status.running ? 'green' : 'slate'}>{status.running ? '网关运行中' : '网关未运行'}</Badge>
            <Badge tone="blue">Core schema v{status.schema_version}</Badge>
            <Badge tone={status.core_mode === 'enforce' ? 'violet' : 'slate'}>模式：{status.core_mode}</Badge>
            <span>外键：{status.foreign_keys_enabled ? '已启用' : '未启用'}</span>
            <span className="max-w-72 truncate" title={status.database_path}>数据库：{status.database_path}</span>
          </div>
        )}
        {scheduler && <div className="mt-2 flex flex-wrap gap-2 text-xs text-slate-500 dark:text-zinc-400"><Badge tone="blue">账号 {scheduler.enabled_accounts}/{scheduler.accounts}</Badge><Badge tone="green">Fresh {scheduler.fresh_observations}</Badge><Badge tone="amber">Stale {scheduler.stale_observations}</Badge><Badge tone={scheduler.unknown_leases > 0 ? 'amber' : 'slate'}>Unknown lease {scheduler.unknown_leases}</Badge><span className="self-center">活动 lease {scheduler.active_leases} · 槽位饱和 {scheduler.slot_saturated} · reader 失败 {scheduler.reader_failures}</span></div>}
        {error && <p className="mt-2 text-xs text-rose-600 dark:text-rose-400">{error}</p>}
      </div>

      {!hasAdminKey && users.length === 0 ? (
        <div className="card flex flex-col items-center justify-center gap-2 p-8 text-center">
          <LockKeyhole size={24} className="text-slate-400" />
          <p className="text-sm font-medium text-slate-600 dark:text-zinc-300">请输入管理员 Key 后加载 Core 数据</p>
          <p className="text-xs text-slate-400">不会尝试使用用户 Key、用户 ID 或 legacy API Key。</p>
        </div>
      ) : (
        <>
          <div className="grid gap-3 xl:grid-cols-2">
            <div className="card p-4">
              <div className="mb-3 flex items-center gap-2">
                <UserRound size={16} className="text-brand-500" />
                <h4 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">创建用户</h4>
              </div>
              <div className="grid gap-2 sm:grid-cols-[1fr_1fr_8rem_auto] sm:items-end">
                <label className="text-xs text-slate-500">用户 ID<input className="input mt-1" value={newUserId} onChange={(event) => setNewUserId(event.target.value)} placeholder="team-a" /></label>
                <label className="text-xs text-slate-500">名称<input className="input mt-1" value={newUserName} onChange={(event) => setNewUserName(event.target.value)} placeholder="团队 A" /></label>
                <label className="text-xs text-slate-500">角色<select className="input mt-1" value={newUserRole} onChange={(event) => setNewUserRole(event.target.value)}><option value="user">user</option><option value="operator">operator</option><option value="admin">admin</option></select></label>
                <button className="btn-outline flex items-center justify-center gap-1 !px-3 text-xs" onClick={() => void createUser()} disabled={busy || !hasAdminKey}><Plus size={14} />创建</button>
              </div>
            </div>

            <div className="card p-4">
              <div className="mb-3 flex items-center gap-2">
                <KeyRound size={16} className="text-brand-500" />
                <h4 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">签发 Core Key</h4>
              </div>
              <div className="grid gap-2 sm:grid-cols-[1fr_1fr_auto] sm:items-end">
                <label className="text-xs text-slate-500">归属用户<select className="input mt-1" value={selectedUserId} onChange={(event) => setSelectedUserId(event.target.value)}><option value="">请选择</option>{users.map((user) => <option key={user.id} value={user.id}>{user.name}（{user.id}）</option>)}</select></label>
                <label className="text-xs text-slate-500">名称 / scopes<input className="input mt-1" value={newKeyName} onChange={(event) => setNewKeyName(event.target.value)} placeholder="worker；video.submit,chat" /></label>
                <button className="btn-outline flex items-center justify-center gap-1 !px-3 text-xs" onClick={() => void issueKey()} disabled={busy || !hasAdminKey}><Plus size={14} />签发</button>
              </div>
              <p className="mt-1 text-[11px] text-slate-400">Scopes 可用逗号分隔；留空表示不附加 scope。</p>
            </div>
          </div>

          <div className="card p-4">
            <div className="mb-3 flex flex-wrap items-end justify-between gap-2">
              <div>
                <div className="flex items-center gap-2"><ListChecks size={16} className="text-brand-500" /><h4 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">视频持久队列</h4><Badge tone="slate">脱敏投影</Badge></div>
                <p className="mt-1 text-xs text-slate-400">只显示归属、状态、租约和额度摘要；不显示 prompt、输入摘要、账号标识或结果路径。</p>
              </div>
              <label className="text-xs text-slate-500">状态<select className="input mt-1 !w-36" value={jobStateFilter} onChange={(event) => setJobStateFilter(event.target.value)}><option value="">全部</option><option value="queued">queued</option><option value="running">running</option><option value="unknown">unknown</option><option value="succeeded">succeeded</option><option value="failed">failed</option><option value="canceled">canceled</option></select></label>
            </div>
            <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400"><th className="pb-2 pr-3">任务</th><th className="pb-2 pr-3">用户</th><th className="pb-2 pr-3">模型</th><th className="pb-2 pr-3">状态</th><th className="pb-2 pr-3">额度</th><th className="pb-2 pr-3">尝试 / lease</th><th className="pb-2">更新时间</th></tr></thead><tbody>{videoJobs.map((job) => <tr key={job.id} className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"><td className="max-w-48 truncate py-2 pr-3 font-mono text-xs" title={job.id}>{job.id}</td><td className="py-2 pr-3 font-mono text-xs">{job.user_id}</td><td className="py-2 pr-3 text-xs">{job.model}</td><td className="py-2 pr-3"><div className="flex flex-wrap items-center gap-1"><Badge tone={coreJobTone(job.state)}>{job.state}</Badge>{job.reconcile_required && <Badge tone="amber">待对账</Badge>}{job.queue_claimed && <Badge tone="blue">已领取</Badge>}</div></td><td className="py-2 pr-3 text-xs">{job.quota_amount ?? '—'} / {job.quota_state ?? '—'}</td><td className="py-2 pr-3 text-xs text-slate-500">{job.attempt_state ?? '—'} / {job.lease_state ?? '—'}</td><td className="py-2 text-xs text-slate-400">{formatTime(job.updated_at_ms)}</td></tr>)}</tbody></table>{videoJobs.length === 0 && <p className="py-4 text-center text-xs text-slate-400">暂无匹配的视频任务。</p>}</div>
            {videoJobs.some((job) => job.state === 'unknown' || job.reconcile_required) && <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">unknown / 待对账任务保留额度，不会由管理界面自动换号重放。</p>}
          </div>

          <div className="card p-4">
            <div className="mb-3 flex flex-wrap items-end justify-between gap-2">
              <div>
                <div className="flex items-center gap-2"><Database size={16} className="text-brand-500" /><h4 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">用户与额度账本</h4></div>
                <p className="mt-1 text-xs text-slate-400">每种 resource_kind 独立计账；available 与 held 不合并。</p>
              </div>
              <div className="flex items-end gap-2">
                <label className="text-xs text-slate-500">资源类型<input className="input mt-1 !w-32" value={resourceKind} onChange={(event) => setResourceKind(event.target.value)} /></label>
                <button className="btn-ghost !px-3 text-xs" onClick={() => void readBalance()} disabled={!selectedUserId || !hasAdminKey}>查余额</button>
              </div>
            </div>
            <div className="mb-3 grid gap-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50 sm:grid-cols-[10rem_1fr_1fr_auto] sm:items-end">
              <label className="text-xs text-slate-500">用户<select className="input mt-1" value={selectedUserId} onChange={(event) => setSelectedUserId(event.target.value)}><option value="">请选择</option>{users.map((user) => <option key={user.id} value={user.id}>{user.id}</option>)}</select></label>
              <label className="text-xs text-slate-500">增加额度<input className="input mt-1" type="number" min={1} value={grantAmount} onChange={(event) => setGrantAmount(event.target.value)} placeholder="100" /></label>
              <label className="text-xs text-slate-500">原因<input className="input mt-1" value={grantReason} onChange={(event) => setGrantReason(event.target.value)} placeholder="月度配额" /></label>
              <button className="btn-outline flex items-center justify-center gap-1 !px-3 text-xs" onClick={() => void grantQuota()} disabled={busy || !hasAdminKey}><Plus size={14} />发放</button>
            </div>
            {balance && <div className="mb-3 flex flex-wrap gap-2 text-xs"><Badge tone="green">可用 {balance.available}</Badge><Badge tone="amber">占用 {balance.held}</Badge><span className="self-center text-slate-400">{balance.user_id} / {balance.resource_kind}</span></div>}
            <div className="overflow-x-auto">
              <table className="w-full text-sm"><thead><tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400"><th className="pb-2 pr-3">ID</th><th className="pb-2 pr-3">名称</th><th className="pb-2 pr-3">角色</th><th className="pb-2 pr-3">状态</th><th className="pb-2 pr-3">更新时间</th><th className="pb-2">操作</th></tr></thead><tbody>{users.map((user) => <tr key={user.id} className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"><td className="py-2 pr-3 font-mono text-xs">{user.id}</td><td className="py-2 pr-3">{user.name}</td><td className="py-2 pr-3"><Badge tone={user.role === 'admin' ? 'violet' : 'slate'}>{user.role}</Badge></td><td className="py-2 pr-3"><Badge tone={statusTone(user.status)}>{user.status}</Badge></td><td className="py-2 pr-3 text-xs text-slate-400">{formatTime(user.updated_at_ms)}</td><td className="py-2"><button className="btn-ghost !px-2 text-xs" onClick={() => setConfirmAction({ kind: 'user', user, active: user.status !== 'active' })}>{user.status === 'active' ? '禁用' : '启用'}</button></td></tr>)}</tbody></table>
              {users.length === 0 && <p className="py-4 text-center text-xs text-slate-400">暂无用户，或管理员 Key 尚未加载。</p>}
            </div>
          </div>

          <div className="card p-4">
            <div className="mb-3 flex flex-wrap items-center justify-between gap-2"><div className="flex items-center gap-2"><KeyRound size={16} className="text-brand-500" /><h4 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">Core API Keys</h4><span className="text-xs text-slate-400">只展示 prefix、scope 和状态，不返回 digest 或明文</span></div><select className="input !w-48 !py-1.5 text-xs" value={keyFilterUserId} onChange={(event) => setKeyFilterUserId(event.target.value)}><option value="">全部用户</option>{users.map((user) => <option key={user.id} value={user.id}>{user.name}（{user.id}）</option>)}</select></div>
            <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400"><th className="pb-2 pr-3">名称</th><th className="pb-2 pr-3">归属</th><th className="pb-2 pr-3">Prefix</th><th className="pb-2 pr-3">Scopes</th><th className="pb-2 pr-3">状态</th><th className="pb-2 pr-3">创建时间</th><th className="pb-2">操作</th></tr></thead><tbody>{keys.map((key) => <tr key={key.id} className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"><td className="py-2 pr-3">{key.name}</td><td className="py-2 pr-3 font-mono text-xs">{key.user_id}</td><td className="py-2 pr-3 font-mono text-xs text-slate-500">{key.prefix}</td><td className="max-w-56 py-2 pr-3 text-xs text-slate-500">{key.scopes.length ? key.scopes.join(', ') : '—'}</td><td className="py-2 pr-3"><Badge tone={statusTone(key.status)}>{key.status}</Badge></td><td className="py-2 pr-3 text-xs text-slate-400">{formatTime(key.created_at_ms)}</td><td className="py-2">{key.status === 'active' ? <button className="btn-ghost !px-2 text-xs text-rose-600" onClick={() => setConfirmAction({ kind: 'key', key })}>撤销</button> : <span className="text-xs text-slate-400">不可用</span>}</td></tr>)}</tbody></table>{keys.length === 0 && <p className="py-4 text-center text-xs text-slate-400">暂无匹配的 Core Key。</p>}</div>
          </div>
        </>
      )}

      <Modal
        open={confirmAction != null}
        onClose={() => setConfirmAction(null)}
        title={confirmAction?.kind === 'key' ? '确认撤销 Core Key' : confirmAction?.kind === 'grant' ? '确认发放 Core 额度' : '确认变更用户状态'}
        footer={<><button className="btn-ghost" onClick={() => setConfirmAction(null)}>取消</button><button className="btn-danger" onClick={() => void confirmMutation()}>确认</button></>}
      >
        {confirmAction?.kind === 'key'
          ? <>将撤销「{confirmAction.key.name}」（{confirmAction.key.prefix}），撤销后不能恢复。</>
          : confirmAction?.kind === 'grant'
            ? <>将向用户「{confirmAction.userId}」发放 {confirmAction.amount} 点「{confirmAction.resourceKind}」额度。原因：{confirmAction.reason}</>
            : <>将{confirmAction?.active ? '启用' : '禁用'}用户「{confirmAction?.user.name}」。最后一个管理员或当前管理员不能被禁用。</>}
      </Modal>

      <Modal
        open={issuedKey != null}
        onClose={() => setIssuedKey(null)}
        title="Core Key 已签发"
        footer={<><button className="btn-ghost" onClick={() => setIssuedKey(null)}>关闭</button><button className="btn-primary flex items-center gap-1" onClick={() => void copyIssuedKey()}><Copy size={14} />复制明文</button></>}
      >
        <div className="space-y-3"><p className="text-xs text-amber-700 dark:text-amber-300">这是唯一一次展示明文的页面。关闭后只能看到 prefix，服务端不会再次返回明文。</p><div className="flex items-center gap-2 rounded-lg bg-slate-100 p-3 font-mono text-xs text-slate-700 dark:bg-zinc-800 dark:text-zinc-200"><span className="min-w-0 flex-1 break-all">{issuedKey?.plaintext}</span><button className="btn-ghost !p-1.5" onClick={() => void copyIssuedKey()} aria-label="复制 Core Key"><Copy size={14} /></button></div><p className="text-xs text-slate-400">归属用户：{issuedKey?.user_id}；请用安全方式保存此 Key。</p></div>
      </Modal>
    </div>
  );
}

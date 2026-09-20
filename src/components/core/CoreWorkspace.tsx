import { useEffect, useState } from 'react';
import { KeyRound, LogOut, LockKeyhole } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import type {
  CoreIssuedApiKeyResponse,
  CoreKeyQuotaBalanceResponse,
  CoreMigrationApplyResponse,
  CoreMigrationMapping,
  CoreMigrationReport,
  CoreQuotaBalanceResponse,
} from '../../types';
import { emptyCoreAdminSession, useCoreAdminSession, type CoreAdminSessionState } from './useCoreAdminSession';
import CoreNav, { CORE_WORKSPACE_SECTIONS, type CoreSectionKey } from './CoreNav';
import CoreStatusBar from './CoreStatusBar';
import CoreOverviewPanel from './CoreOverviewPanel';
import CoreUsersKeysPanel from './CoreUsersKeysPanel';
import CoreQuotaPanel from './CoreQuotaPanel';
import CoreJobsPanel from './CoreJobsPanel';
import CoreMigrationPanel from './CoreMigrationPanel';

export { CORE_WORKSPACE_SECTIONS };
export { emptyCoreAdminSession };

export function clearCoreAdminSession(session: CoreAdminSessionState): CoreAdminSessionState {
  return {
    ...emptyCoreAdminSession,
    status: session.status,
  };
}

function sectionSummary(section: CoreSectionKey, counts: { users: number; keys: number; jobs: number }): string {
  if (section === 'users') return `${counts.users} 个用户 · ${counts.keys} 个 API Key`;
  if (section === 'jobs') return `${counts.jobs} 个脱敏视频任务`;
  if (section === 'quota') return '额度读取与发放操作将在此集中管理';
  if (section === 'migration') return '迁移前先 inspect，报告未通过时不允许 apply';
  return '管理员会话、调度健康与能力状态';
}

export default function CoreWorkspace() {
  const session = useCoreAdminSession();
  const [active, setActive] = useState<CoreSectionKey>('overview');
  const [keyInput, setKeyInput] = useState('');
  const [userBalance, setUserBalance] = useState<CoreQuotaBalanceResponse | null>(null);
  const [keyBalance, setKeyBalance] = useState<CoreKeyQuotaBalanceResponse | null>(null);
  const [migrationReport, setMigrationReport] = useState<CoreMigrationReport | null>(null);
  const [actionError, setActionError] = useState('');
  const [busyAction, setBusyAction] = useState(false);

  useEffect(() => {
    void session.refreshStatus().catch(() => undefined);
  }, [session.refreshStatus]);

  const submitKey = async () => {
    const normalized = keyInput.trim();
    if (!normalized) return;
    session.setAdminApiKey(normalized);
    await session.loadAll(normalized);
  };

  const runMutation = async <T,>(action: () => Promise<T>) => {
    setBusyAction(true);
    setActionError('');
    try {
      const result = await action();
      await session.loadAll();
      return result;
    } catch (error) {
      setActionError(String(error).replace(/^Error:\s*/, '').slice(0, 180));
      return null;
    } finally {
      setBusyAction(false);
    }
  };

  const adminKey = () => session.adminApiKey.trim();
  const loadUserBalance = async (userId: string, resourceKind: string) => {
    const result = await runMutation(() => api.core.quotaBalance(adminKey(), userId, resourceKind));
    if (result) setUserBalance(result);
  };
  const loadKeyBalance = async (apiKeyId: string, resourceKind: string) => {
    const result = await runMutation(() => api.core.keyQuotaBalance(adminKey(), apiKeyId, resourceKind));
    if (result) setKeyBalance(result);
  };
  const createUser = (id: string, name: string, role: string) => runMutation(() => api.core.userCreate(adminKey(), id, name, role)).then(() => undefined);
  const issueKey = (userId: string, name: string, scopes: string[]) => runMutation(() => api.core.apiKeyIssue(adminKey(), userId, name, scopes)) as Promise<CoreIssuedApiKeyResponse | null>;
  const setUserStatus = (userId: string, activeStatus: boolean) => runMutation(() => api.core.userSetStatus(adminKey(), userId, activeStatus)).then(() => undefined);
  const revokeKey = (keyId: string) => runMutation(() => api.core.apiKeyRevoke(adminKey(), keyId)).then(() => undefined);
  const grantUser = (userId: string, resourceKind: string, amount: number, reason: string) => runMutation(() => api.core.quotaGrant(adminKey(), userId, resourceKind, amount, reason)).then(() => undefined);
  const grantKey = (apiKeyId: string, resourceKind: string, amount: number, reason: string) => runMutation(() => api.core.keyQuotaGrant(adminKey(), apiKeyId, resourceKind, amount, reason)).then(() => undefined);
  const migrateQuota = (sourceUserId: string, apiKeyId: string, resourceKind: string, amount: number, migrationId: string, reason: string) => runMutation(() => api.core.keyQuotaAllocateLegacy(adminKey(), sourceUserId, apiKeyId, resourceKind, amount, migrationId, reason)).then(() => undefined);
  const inspectMigration = async () => {
    const result = await runMutation(() => api.core.migrationInspect());
    if (result) setMigrationReport(result);
    return result;
  };
  const applyMigration = (mappings: CoreMigrationMapping[]) => runMutation(() => api.core.migrationApply(adminKey(), mappings)) as Promise<CoreMigrationApplyResponse | null>;

  const counts = { users: session.users.length, keys: session.keys.length, jobs: session.videoJobs.length };

  return (
    <div className="mx-auto flex w-full max-w-[1200px] flex-col gap-4 pb-8">
      <CoreStatusBar
        status={session.status}
        scheduler={session.scheduler}
        loading={session.loading}
        onRefresh={() => { void session.refreshStatus().catch(() => undefined); if (session.hasAdminKey) void session.loadAll(); }}
      />
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold text-slate-800 dark:text-zinc-100">独立 CORE 管理工作台</h1>
          <p className="mt-1 text-sm text-slate-500 dark:text-zinc-400">身份、额度、任务和迁移统一通过管理命令读取，不直接访问 Core 数据库。</p>
        </div>
        {session.hasAdminKey && (
          <button className="btn-secondary flex items-center gap-2" onClick={session.clearSession}>
            <LogOut size={15} /> 清除管理员会话
          </button>
        )}
      </div>
      {!session.hasAdminKey && (
        <section className="card border-brand-200 p-5 dark:border-brand-900/60">
          <div className="flex items-start gap-3">
            <LockKeyhole className="mt-0.5 text-brand-500" size={20} />
            <div className="min-w-0 flex-1">
              <h2 className="font-medium text-slate-800 dark:text-zinc-100">输入 Core 管理员 API Key</h2>
              <p className="mt-1 text-xs text-slate-500 dark:text-zinc-400">Key 只保存在当前页面内存中，清除会话或关闭程序后不会保留。</p>
              <div className="mt-3 flex gap-2">
                <input
                  className="input min-w-0 flex-1"
                  type="password"
                  value={keyInput}
                  onChange={(event) => setKeyInput(event.target.value)}
                  onKeyDown={(event) => { if (event.key === 'Enter') void submitKey(); }}
                  placeholder="Core 管理员 API Key"
                  autoComplete="off"
                />
                <button className="btn-primary flex items-center gap-2" onClick={() => void submitKey()} disabled={session.loading}>
                  {session.loading ? <Spinner /> : <KeyRound size={15} />} 连接
                </button>
              </div>
            </div>
          </div>
        </section>
      )}
      {(session.error || actionError) && <div className="rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-sm text-rose-700 dark:border-rose-900/60 dark:bg-rose-950/30 dark:text-rose-300">{session.error || actionError}</div>}
      <CoreNav active={active} onChange={setActive} />
      <section className="card min-h-[320px] p-5">
        <div className="flex items-center justify-between gap-3">
          <div>
            <h2 className="text-base font-semibold text-slate-800 dark:text-zinc-100">
              {CORE_WORKSPACE_SECTIONS.find((section) => section.key === active)?.label}
            </h2>
            <p className="mt-1 text-xs text-slate-500 dark:text-zinc-400">{sectionSummary(active, counts)}</p>
          </div>
          {session.loading && <Badge tone="blue"><Spinner className="mr-1 align-[-2px]" />刷新中</Badge>}
        </div>
        <div className="mt-6">
          {active === 'overview' && <CoreOverviewPanel status={session.status} scheduler={session.scheduler} users={session.users} keys={session.keys} videoJobs={session.videoJobs} />}
          {active === 'users' && <CoreUsersKeysPanel users={session.users} keys={session.keys} busy={busyAction} onCreateUser={createUser} onIssueKey={issueKey} onSetUserStatus={setUserStatus} onRevokeKey={revokeKey} />}
          {active === 'quota' && <CoreQuotaPanel users={session.users} keys={session.keys} userBalance={userBalance} keyBalance={keyBalance} onLoadUserBalance={loadUserBalance} onLoadKeyBalance={loadKeyBalance} onGrantUser={grantUser} onGrantKey={grantKey} onMigrate={migrateQuota} />}
          {active === 'jobs' && <CoreJobsPanel jobs={session.videoJobs} onRefresh={() => { void session.loadAll(); }} />}
          {active === 'migration' && <CoreMigrationPanel report={migrationReport} onInspect={inspectMigration} onApply={applyMigration} />}
        </div>
      </section>
    </div>
  );
}

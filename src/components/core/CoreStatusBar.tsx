import { Database, RefreshCw, ShieldCheck } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import type { CoreSchedulerStatus, CoreStatus } from '../../types';

export default function CoreStatusBar({
  status,
  scheduler,
  loading,
  onRefresh,
}: {
  status: CoreStatus | null;
  scheduler: CoreSchedulerStatus | null;
  loading: boolean;
  onRefresh: () => void;
}) {
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-xl border border-slate-200 bg-white px-4 py-3 shadow-sm dark:border-zinc-800 dark:bg-zinc-900">
      <div className="mr-2 flex items-center gap-2 text-sm font-semibold text-slate-800 dark:text-zinc-100">
        <Database size={17} /> CORE 工作台
      </div>
      <Badge tone={status?.running ? 'green' : 'amber'}>
        {status?.running ? '网关运行中' : '网关未运行'}
      </Badge>
      <Badge tone={status?.core_mode === 'enforce' ? 'blue' : 'slate'}>
        Core {status?.core_mode ?? '未知'}
      </Badge>
      {scheduler && (
        <span className="text-xs text-slate-500 dark:text-zinc-400">
          调度：{scheduler.enabled_accounts}/{scheduler.accounts} 个账号可用 · 活跃租约 {scheduler.active_leases}
        </span>
      )}
      <span className="ml-auto flex items-center gap-2 text-xs text-slate-400">
        <ShieldCheck size={14} /> schema {status?.schema_version ?? '—'}
        <button className="btn-ghost h-7 !px-2" onClick={onRefresh} disabled={loading} title="刷新 CORE 状态">
          {loading ? <Spinner /> : <RefreshCw size={14} />}
        </button>
      </span>
    </div>
  );
}

import { Activity, KeyRound, Users, Video } from 'lucide-react';
import { Badge, StatCard } from '../ui';
import type { CoreApiKeyAdminView, CoreSchedulerStatus, CoreStatus, CoreUserAdminView, CoreVideoJobAdminView } from '../../types';

export default function CoreOverviewPanel({
  status,
  scheduler,
  users,
  keys,
  videoJobs,
}: {
  status: CoreStatus | null;
  scheduler: CoreSchedulerStatus | null;
  users: CoreUserAdminView[];
  keys: CoreApiKeyAdminView[];
  videoJobs: CoreVideoJobAdminView[];
}) {
  return (
    <div className="space-y-4">
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <StatCard label="用户" value={users.length} hint="Core 身份记录" tone="blue" />
        <StatCard label="API Key" value={keys.length} hint="列表只显示脱敏前缀" tone="violet" />
        <StatCard label="视频任务" value={videoJobs.length} hint="管理员脱敏投影" tone="amber" />
        <StatCard label="活动租约" value={scheduler?.active_leases ?? '—'} hint={status?.core_mode ?? 'Core 状态未知'} tone="green" />
      </div>
      <div className="grid gap-4 lg:grid-cols-2">
        <div className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800">
          <div className="flex items-center gap-2 font-medium"><Activity size={16} /> 调度健康</div>
          <div className="mt-3 grid grid-cols-2 gap-2 text-sm">
            <div>账号：{scheduler?.accounts ?? '—'}</div>
            <div>启用：{scheduler?.enabled_accounts ?? '—'}</div>
            <div>新鲜观测：{scheduler?.fresh_observations ?? '—'}</div>
            <div>过期观测：{scheduler?.stale_observations ?? '—'}</div>
            <div>未知租约：{scheduler?.unknown_leases ?? '—'}</div>
            <div>槽位饱和：{scheduler?.slot_saturated ?? '—'}</div>
          </div>
        </div>
        <div className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800">
          <div className="flex items-center gap-2 font-medium"><Users size={16} /> 管理边界</div>
          <p className="mt-3 text-sm text-slate-500 dark:text-zinc-400">管理员 Key 只存在当前 React 会话内；Key 列表不返回明文，任务列表不返回 prompt、凭据或本地结果路径。</p>
          <div className="mt-3 flex flex-wrap gap-2"><Badge tone="green"><KeyRound size={12} className="mr-1 inline" />Key 明文不落盘</Badge><Badge tone="blue"><Video size={12} className="mr-1 inline" />视频任务脱敏</Badge></div>
        </div>
      </div>
    </div>
  );
}

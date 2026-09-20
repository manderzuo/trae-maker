import { cn } from '../../lib/cn';

export type CoreSectionKey = 'overview' | 'users' | 'quota' | 'jobs' | 'migration';

export const CORE_WORKSPACE_SECTIONS: { key: CoreSectionKey; label: string; hint: string }[] = [
  { key: 'overview', label: '总览', hint: '运行状态与风险摘要' },
  { key: 'users', label: '用户与 Key', hint: '身份、scope 与撤销' },
  { key: 'quota', label: '额度中心', hint: '可用、占用与已结算' },
  { key: 'jobs', label: '任务与租约', hint: '视频任务和对账状态' },
  { key: 'migration', label: '迁移', hint: 'Legacy 数据检查与迁移' },
];

export default function CoreNav({
  active,
  onChange,
}: {
  active: CoreSectionKey;
  onChange: (section: CoreSectionKey) => void;
}) {
  return (
    <nav className="flex flex-wrap gap-1 rounded-xl border border-slate-200 bg-white p-1 dark:border-zinc-800 dark:bg-zinc-900">
      {CORE_WORKSPACE_SECTIONS.map((section) => (
        <button
          key={section.key}
          onClick={() => onChange(section.key)}
          className={cn(
            'rounded-lg px-3 py-2 text-left text-sm transition',
            active === section.key
              ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
              : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800',
          )}
          title={section.hint}
        >
          {section.label}
        </button>
      ))}
    </nav>
  );
}

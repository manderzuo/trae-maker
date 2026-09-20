import { useState } from 'react';
import { Search, Upload } from 'lucide-react';
import { Badge } from '../ui';
import type { CoreIssuedApiKeyResponse, CoreMigrationApplyResponse, CoreMigrationMapping, CoreMigrationReport } from '../../types';
import { coreMigrationTone, shouldAllowMigrationApply } from './coreModel';

export default function CoreMigrationPanel({
  report,
  onInspect,
  onApply,
}: {
  report: CoreMigrationReport | null;
  onInspect: () => Promise<CoreMigrationReport | null>;
  onApply: (mappings: CoreMigrationMapping[]) => Promise<CoreMigrationApplyResponse | null>;
}) {
  const [mapping, setMapping] = useState<Record<string, string>>({});
  const [issued, setIssued] = useState<CoreIssuedApiKeyResponse[]>([]);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const inspect = async () => { setBusy(true); try { const next = await onInspect(); if (next) setMapping(Object.fromEntries(next.unmapped_keys.map((key) => [key, mapping[key] ?? '']))); } finally { setBusy(false); } };
  const apply = async () => { setConfirm(false); setBusy(true); try { const result = await onApply(Object.entries(mapping).filter(([, userId]) => userId.trim()).map(([legacy_key_id, user_id]) => ({ legacy_key_id, user_id: user_id.trim() }))); if (result) setIssued(result.issued_keys); } finally { setBusy(false); } };
  return <div className="space-y-4"><div className="flex flex-wrap items-center gap-2"><button className="btn-secondary flex items-center gap-2" disabled={busy} onClick={() => void inspect()}>{busy ? '检查中…' : <><Search size={14} />inspect Legacy</>}</button>{report && <Badge tone={coreMigrationTone(report)}>{coreMigrationTone(report) === 'green' ? '可继续' : '需要处理'}</Badge>}</div>{report && <><div className="grid gap-3 sm:grid-cols-3"><div className="card p-3 text-xs">Key：{report.key_count}</div><div className="card p-3 text-xs">素材：{report.asset_count}</div><div className="card p-3 text-xs">视频：{report.video_count}</div></div>{report.errors.length > 0 && <div className="rounded-xl border border-rose-200 bg-rose-50 p-3 text-xs text-rose-700 dark:border-rose-900/60 dark:bg-rose-950/30 dark:text-rose-300">{report.errors.join('；')}</div>}{report.unmapped_keys.length > 0 && <div className="rounded-xl border border-amber-200 bg-amber-50 p-4 dark:border-amber-900/60 dark:bg-amber-950/20"><div className="text-sm font-medium">为未映射的 Legacy Key 指定 Core 用户</div><div className="mt-3 space-y-2">{report.unmapped_keys.map((legacyKey) => <div key={legacyKey} className="grid gap-2 sm:grid-cols-2"><code className="rounded bg-white px-2 py-2 text-xs dark:bg-zinc-900">{legacyKey}</code><input className="input" value={mapping[legacyKey] ?? ''} onChange={(event) => setMapping((current) => ({ ...current, [legacyKey]: event.target.value }))} placeholder="Core 用户 ID" /></div>)}</div></div>}<button className="btn-primary flex items-center gap-2" disabled={busy || !shouldAllowMigrationApply(report) || report.unmapped_keys.some((key) => !mapping[key]?.trim())} onClick={() => setConfirm(true)}><Upload size={14} />应用迁移</button></>}{confirm && <div className="rounded-xl border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-200">迁移会写入 Core 身份、Key、素材和任务映射，且不可逆。<div className="mt-2 flex gap-2"><button className="btn-primary" onClick={() => void apply()}>确认应用</button><button className="btn-secondary" onClick={() => setConfirm(false)}>取消</button></div></div>}{issued.length > 0 && <div className="rounded-xl border border-emerald-300 bg-emerald-50 p-3 text-xs dark:border-emerald-800 dark:bg-emerald-950/30"><div className="font-medium">迁移签发的 Key 明文仅显示本次结果</div>{issued.map((key) => <div key={key.id} className="mt-2 break-all"><span className="text-slate-500">{key.prefix}</span> <code>{key.plaintext}</code></div>)}</div>}</div>;
}

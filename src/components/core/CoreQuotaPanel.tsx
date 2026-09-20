import { useState } from 'react';
import { Coins } from 'lucide-react';
import { Badge, StatCard } from '../ui';
import type { CoreApiKeyAdminView, CoreKeyQuotaBalanceResponse, CoreQuotaBalanceResponse, CoreUserAdminView } from '../../types';
import { coreQuotaSummary } from './coreModel';

export default function CoreQuotaPanel({
  users,
  keys,
  userBalance,
  keyBalance,
  onLoadUserBalance,
  onLoadKeyBalance,
  onGrantUser,
  onGrantKey,
  onMigrate,
}: {
  users: CoreUserAdminView[];
  keys: CoreApiKeyAdminView[];
  userBalance: CoreQuotaBalanceResponse | null;
  keyBalance: CoreKeyQuotaBalanceResponse | null;
  onLoadUserBalance: (userId: string, resourceKind: string) => Promise<void>;
  onLoadKeyBalance: (keyId: string, resourceKind: string) => Promise<void>;
  onGrantUser: (userId: string, resourceKind: string, amount: number, reason: string) => Promise<void>;
  onGrantKey: (keyId: string, resourceKind: string, amount: number, reason: string) => Promise<void>;
  onMigrate: (sourceUserId: string, keyId: string, resourceKind: string, amount: number, migrationId: string, reason: string) => Promise<void>;
}) {
  const [userId, setUserId] = useState(users[0]?.id ?? '');
  const [keyId, setKeyId] = useState(keys[0]?.id ?? '');
  const [kind, setKind] = useState('video_job');
  const [amount, setAmount] = useState('');
  const [reason, setReason] = useState('');
  const [migrationId, setMigrationId] = useState('');
  const [confirm, setConfirm] = useState<(() => Promise<void>) | null>(null);
  const summary = keyBalance ? coreQuotaSummary(keyBalance) : null;
  const confirmRun = async () => { if (!confirm) return; const action = confirm; setConfirm(null); await action(); };

  return (
    <div className="space-y-5">
      {confirm && <div className="rounded-xl border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-200"><div>额度变更不可逆，请确认对象、资源类型、数量和原因。</div><div className="mt-2 flex gap-2"><button className="btn-primary" onClick={() => void confirmRun()}>确认发放</button><button className="btn-secondary" onClick={() => setConfirm(null)}>取消</button></div></div>}
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">{summary ? <><StatCard label="总额度" value={summary.total} tone="blue" /><StatCard label="可用" value={summary.available} tone="green" /><StatCard label="held" value={summary.held} tone="amber" /><StatCard label="已结算" value={summary.settled} tone="violet" /></> : <div className="sm:col-span-2 xl:col-span-4 rounded-xl border border-dashed border-slate-300 p-6 text-center text-sm text-slate-500 dark:border-zinc-700">选择 Key 并读取额度后展示四项账本状态</div>}</div>
      <div className="grid gap-4 lg:grid-cols-2"><section className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800"><div className="flex items-center gap-2 font-medium"><Coins size={16} /> 查询额度</div><div className="mt-3 grid gap-2 sm:grid-cols-3"><select className="input" value={userId} onChange={(event) => setUserId(event.target.value)}><option value="">用户</option>{users.map((user) => <option key={user.id} value={user.id}>{user.name}</option>)}</select><select className="input" value={keyId} onChange={(event) => setKeyId(event.target.value)}><option value="">API Key</option>{keys.map((key) => <option key={key.id} value={key.id}>{key.prefix}</option>)}</select><input className="input" value={kind} onChange={(event) => setKind(event.target.value)} placeholder="资源类型" /></div><div className="mt-3 flex gap-2"><button className="btn-secondary" disabled={!userId} onClick={() => void onLoadUserBalance(userId, kind)}>查用户额度</button><button className="btn-secondary" disabled={!keyId} onClick={() => void onLoadKeyBalance(keyId, kind)}>查 Key 额度</button></div>{userBalance && <div className="mt-3 text-xs text-slate-500">用户 {userBalance.user_id} · 可用 {userBalance.available} · held {userBalance.held}</div>}{keyBalance && <div className="mt-1 text-xs text-slate-500">Key {keyBalance.api_key_id} · 状态 {keyBalance.migration_state}</div>}</section><section className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800"><div className="font-medium">发放与迁移</div><div className="mt-3 grid gap-2 sm:grid-cols-2"><input className="input" type="number" min="1" value={amount} onChange={(event) => setAmount(event.target.value)} placeholder="数量" /><input className="input" value={reason} onChange={(event) => setReason(event.target.value)} placeholder="操作原因" /><input className="input" value={migrationId} onChange={(event) => setMigrationId(event.target.value)} placeholder="迁移 ID（迁移时必填）" /></div><div className="mt-3 flex flex-wrap gap-2"><button className="btn-primary" disabled={!userId || !amount || !reason} onClick={() => setConfirm(() => () => onGrantUser(userId, kind, Number(amount), reason))}>发放用户额度</button><button className="btn-secondary" disabled={!keyId || !amount || !reason} onClick={() => setConfirm(() => () => onGrantKey(keyId, kind, Number(amount), reason))}>发放 Key 额度</button><button className="btn-secondary" disabled={!userId || !keyId || !amount || !reason || !migrationId} onClick={() => setConfirm(() => () => onMigrate(userId, keyId, kind, Number(amount), migrationId, reason))}>迁移 Legacy 额度</button></div><p className="mt-2 text-xs text-slate-400">held 是已预留但未结算额度，不会直接计入已结算。</p></section></div>
      <div className="rounded-xl border border-slate-200 p-4 text-xs text-slate-500 dark:border-zinc-800"><Badge tone="blue">真实积分账本</Badge><span className="ml-2">CORE 额度由后端账本控制；前端只展示 available / held / settled，不使用 token 估算。</span></div>
    </div>
  );
}

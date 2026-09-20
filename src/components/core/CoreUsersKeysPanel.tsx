import { useState } from 'react';
import { Copy, KeyRound, UserPlus } from 'lucide-react';
import { Badge } from '../ui';
import type { CoreApiKeyAdminView, CoreIssuedApiKeyResponse, CoreUserAdminView } from '../../types';
import { normalizeCoreScopes } from './coreModel';

const SCOPE_OPTIONS = ['models:read', 'chat:invoke', 'assets:read', 'assets:write', 'videos:read', 'videos:submit', 'usage:read'];

export default function CoreUsersKeysPanel({
  users,
  keys,
  busy,
  onCreateUser,
  onIssueKey,
  onSetUserStatus,
  onRevokeKey,
}: {
  users: CoreUserAdminView[];
  keys: CoreApiKeyAdminView[];
  busy?: boolean;
  onCreateUser: (id: string, name: string, role: string) => Promise<void>;
  onIssueKey: (userId: string, name: string, scopes: string[]) => Promise<CoreIssuedApiKeyResponse | null>;
  onSetUserStatus: (userId: string, active: boolean) => Promise<void>;
  onRevokeKey: (keyId: string) => Promise<void>;
}) {
  const [userId, setUserId] = useState('');
  const [userName, setUserName] = useState('');
  const [role, setRole] = useState('user');
  const [keyUserId, setKeyUserId] = useState(users[0]?.id ?? '');
  const [keyName, setKeyName] = useState('');
  const [scopes, setScopes] = useState<string[]>(['models:read', 'chat:invoke']);
  const [confirm, setConfirm] = useState<{ title: string; action: () => Promise<void> } | null>(null);
  const [issued, setIssued] = useState<CoreIssuedApiKeyResponse | null>(null);

  const createUser = async () => {
    if (!userId.trim() || !userName.trim()) return;
    await onCreateUser(userId.trim(), userName.trim(), role);
    setUserId(''); setUserName('');
  };
  const issueKey = async () => {
    if (!keyUserId.trim() || !keyName.trim()) return;
    const result = await onIssueKey(keyUserId.trim(), keyName.trim(), normalizeCoreScopes(scopes));
    if (result) setIssued(result);
    setKeyName('');
  };
  const toggleScope = (scope: string) => setScopes((current) => normalizeCoreScopes(current.includes(scope) ? current.filter((item) => item !== scope) : [...current, scope]));

  return (
    <div className="space-y-5">
      {confirm && <div className="rounded-xl border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-200"><div className="font-medium">请确认：{confirm.title}</div><div className="mt-2 flex gap-2"><button className="btn-primary" onClick={async () => { const action = confirm.action; setConfirm(null); await action(); }}>确认执行</button><button className="btn-secondary" onClick={() => setConfirm(null)}>取消</button></div></div>}
      {issued && <div className="rounded-xl border border-emerald-300 bg-emerald-50 p-4 text-sm dark:border-emerald-800 dark:bg-emerald-950/30"><div className="font-medium text-emerald-800 dark:text-emerald-200">Key 已签发，仅此处显示一次明文</div><div className="mt-2 flex items-center gap-2"><code className="min-w-0 flex-1 break-all rounded bg-white px-2 py-1 text-xs dark:bg-zinc-900">{issued.plaintext}</code><button className="btn-secondary !p-2" onClick={() => void navigator.clipboard?.writeText(issued.plaintext)} title="复制 Key"><Copy size={14} /></button><button className="btn-secondary" onClick={() => setIssued(null)}>关闭</button></div></div>}
      <div className="grid gap-4 xl:grid-cols-2">
        <section className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800">
          <div className="flex items-center gap-2 font-medium"><UserPlus size={16} /> 新建用户</div>
          <div className="mt-3 grid gap-2 sm:grid-cols-3"><input className="input" value={userId} onChange={(event) => setUserId(event.target.value)} placeholder="用户 ID" /><input className="input" value={userName} onChange={(event) => setUserName(event.target.value)} placeholder="名称" /><select className="input" value={role} onChange={(event) => setRole(event.target.value)}><option value="user">user</option><option value="operator">operator</option><option value="admin">admin</option></select></div>
          <button className="btn-primary mt-3" disabled={busy || !userId.trim() || !userName.trim()} onClick={() => void createUser()}>创建用户</button>
        </section>
        <section className="rounded-xl border border-slate-200 p-4 dark:border-zinc-800">
          <div className="flex items-center gap-2 font-medium"><KeyRound size={16} /> 签发 API Key</div>
          <div className="mt-3 grid gap-2 sm:grid-cols-2"><select className="input" value={keyUserId} onChange={(event) => setKeyUserId(event.target.value)}><option value="">选择用户</option>{users.map((user) => <option key={user.id} value={user.id}>{user.name} · {user.id}</option>)}</select><input className="input" value={keyName} onChange={(event) => setKeyName(event.target.value)} placeholder="Key 名称" /></div>
          <div className="mt-3 flex flex-wrap gap-2">{SCOPE_OPTIONS.map((scope) => <label key={scope} className="flex items-center gap-1 text-xs"><input type="checkbox" checked={scopes.includes(scope)} onChange={() => toggleScope(scope)} />{scope}</label>)}</div>
          <button className="btn-primary mt-3" disabled={busy || !keyUserId || !keyName.trim()} onClick={() => void issueKey()}>签发并显示一次</button>
        </section>
      </div>
      <section><h3 className="mb-2 text-sm font-semibold">用户列表</h3><div className="overflow-x-auto rounded-xl border border-slate-200 dark:border-zinc-800"><table className="w-full text-left text-xs"><thead className="bg-slate-50 dark:bg-zinc-900"><tr><th className="p-3">用户</th><th className="p-3">角色</th><th className="p-3">状态</th><th className="p-3">操作</th></tr></thead><tbody>{users.map((user) => <tr key={user.id} className="border-t border-slate-100 dark:border-zinc-800"><td className="p-3">{user.name}<span className="ml-2 text-slate-400">{user.id}</span></td><td className="p-3">{user.role}</td><td className="p-3"><Badge tone={user.status === 'active' ? 'green' : 'amber'}>{user.status}</Badge></td><td className="p-3"><button className="btn-secondary !px-2 !py-1" onClick={() => setConfirm({ title: `${user.status === 'active' ? '禁用' : '启用'}用户「${user.name}」`, action: () => onSetUserStatus(user.id, user.status !== 'active') })}>{user.status === 'active' ? '禁用' : '启用'}</button></td></tr>)}</tbody></table></div></section>
      <section><h3 className="mb-2 text-sm font-semibold">Key 列表</h3><div className="overflow-x-auto rounded-xl border border-slate-200 dark:border-zinc-800"><table className="w-full text-left text-xs"><thead className="bg-slate-50 dark:bg-zinc-900"><tr><th className="p-3">名称 / 前缀</th><th className="p-3">用户</th><th className="p-3">scope</th><th className="p-3">状态</th><th className="p-3">操作</th></tr></thead><tbody>{keys.map((key) => <tr key={key.id} className="border-t border-slate-100 dark:border-zinc-800"><td className="p-3">{key.name}<span className="ml-2 text-slate-400">{key.prefix}</span></td><td className="p-3">{key.user_id}</td><td className="max-w-[260px] p-3">{key.scopes.join(', ') || '无'}</td><td className="p-3"><Badge tone={key.status === 'active' ? 'green' : 'red'}>{key.status}</Badge></td><td className="p-3">{key.status === 'active' && <button className="btn-secondary !px-2 !py-1" onClick={() => setConfirm({ title: `撤销 Key「${key.prefix}」`, action: () => onRevokeKey(key.id) })}>撤销</button>}</td></tr>)}</tbody></table></div></section>
    </div>
  );
}

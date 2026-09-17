import { useEffect, useState } from 'react';
import { ArrowRight, ExternalLink } from 'lucide-react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { BitBrowserProfile, GroupView } from '../../types';

export function OAuthLoginModal({
  open,
  onClose,
  groups,
  onLogin,
  onBitBrowserLogin,
}: {
  open: boolean;
  onClose: () => void;
  groups: GroupView[];
  onLogin: (
    callbackUrl: string,
    accountName?: string,
    groupId?: string,
  ) => Promise<void>;
  onBitBrowserLogin?: (
    windowId: string,
    accountName?: string,
    groupId?: string,
  ) => Promise<void>;
}) {
  const [step, setStep] = useState(1);
  const [callbackUrl, setCallbackUrl] = useState('');
  const [accountName, setAccountName] = useState('');
  const [gid, setGid] = useState('');
  const [busy, setBusy] = useState(false);
  const [opening, setOpening] = useState(false);
  const [autoWaiting, setAutoWaiting] = useState(false);
  const [autoCallback, setAutoCallback] = useState(false);
  const [bitProfiles, setBitProfiles] = useState<BitBrowserProfile[]>([]);
  const [bitWindowId, setBitWindowId] = useState('');
  const [bitLoading, setBitLoading] = useState(false);
  const [bitBusy, setBitBusy] = useState(false);
  const toast = useAppStore((s) => s.pushToast);

  const loadBitProfiles = async () => {
    setBitLoading(true);
    try {
      const list = await api.accounts.profilesBitBrowser();
      setBitProfiles(list);
      setBitWindowId((current) => current || list.find((profile) => profile.open)?.id || list[0]?.id || '');
    } catch (err) {
      setBitProfiles([]);
      toast('info', `BitBrowser 窗口读取失败：${String(err)}`);
    } finally {
      setBitLoading(false);
    }
  };

  useEffect(() => {
    if (!open) {
      setStep(1);
      setCallbackUrl('');
      setAccountName('');
      setGid('');
      setBusy(false);
      setOpening(false);
      setAutoWaiting(false);
      setAutoCallback(false);
      setBitProfiles([]);
      setBitWindowId('');
      setBitLoading(false);
      setBitBusy(false);
    }
    if (open) void loadBitProfiles();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // 普通浏览器 OAuth 优先自动接收本机回环回调；手动粘贴始终保留作回退。
  useEffect(() => {
    if (!open || step !== 2 || !autoWaiting) return;
    let stopped = false;
    const timer = window.setInterval(() => {
      void api.oauth.pollCallback().then((value) => {
        if (stopped || !value) return;
        setCallbackUrl(value);
        setAutoWaiting(false);
        setAutoCallback(true);
        setStep(3);
        toast('success', '已自动接收 OAuth 回调，可以完成登录');
      }).catch(() => undefined);
    }, 700);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [autoWaiting, open, step, toast]);

  const openLoginPage = async () => {
    setOpening(true);
    try {
      const { url, auto_callback } = await api.oauth.getLoginUrl();
      const { open } = await import('@tauri-apps/plugin-shell');
      await open(url);
      setAutoCallback(auto_callback);
      setAutoWaiting(auto_callback);
      setStep(2);
    } catch (err) {
      toast('error', `获取登录 URL 失败：${String(err)}`);
    } finally {
      setOpening(false);
    }
  };

  const finish = async () => {
    if (!callbackUrl.trim()) return;
    setBusy(true);
    try {
      await onLogin(
        callbackUrl.trim(),
        accountName.trim() || undefined,
        gid || undefined,
      );
    } finally {
      setBusy(false);
    }
  };

  const openRegistrationPage = async () => {
    setOpening(true);
    try {
      const { open } = await import('@tauri-apps/plugin-shell');
      await open('https://www.trae.cn/');
      toast('info', '已打开 Trae 注册入口；验证码、CAPTCHA 与风控验证请在浏览器中手动完成');
    } catch (err) {
      toast('error', `打开注册入口失败：${String(err).slice(0, 120)}`);
    } finally {
      setOpening(false);
    }
  };

  const startBitBrowserLogin = async () => {
    if (!bitWindowId.trim() || !onBitBrowserLogin) return;
    setBitBusy(true);
    try {
      await onBitBrowserLogin(
        bitWindowId.trim(),
        accountName.trim() || undefined,
        gid || undefined,
      );
    } finally {
      setBitBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="OAuth 登录"
      footer={
        <>
          {step > 1 && (
            <button
              onClick={() => setStep((s) => Math.max(1, s - 1))}
              className="btn-ghost"
              disabled={busy || opening || bitBusy}
            >
              上一步
            </button>
          )}
          <button onClick={onClose} className="btn-ghost" disabled={busy || opening || bitBusy}>
            取消
          </button>
          {step === 1 && (
            <button
              onClick={openLoginPage}
              disabled={opening}
              className="btn-primary"
            >
              {opening ? '正在打开...' : '打开登录页'}
              {!opening && <ExternalLink size={14} />}
            </button>
          )}
          {step === 2 && (
            <button
              onClick={() => setStep(3)}
              disabled={!callbackUrl.trim()}
              className="btn-primary"
            >
              下一步 <ArrowRight size={14} />
            </button>
          )}
          {step === 3 && (
            <button
              onClick={finish}
              disabled={busy || !callbackUrl.trim()}
              className="btn-primary"
            >
              {busy ? '登录中...' : '完成登录'}
            </button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {/* 步骤指示器 */}
        <div className="flex items-center gap-2">
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 1
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            1
          </div>
          <div
            className={`h-0.5 w-8 ${step > 1 ? 'bg-brand-500' : 'bg-slate-200 dark:bg-zinc-700'}`}
          />
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 2
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            2
          </div>
          <div
            className={`h-0.5 w-8 ${step > 2 ? 'bg-brand-500' : 'bg-slate-200 dark:bg-zinc-700'}`}
          />
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 3
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            3
          </div>
        </div>

        {step === 1 && (
          <div className="space-y-4 text-sm text-slate-600 dark:text-zinc-300">
            {onBitBrowserLogin && (
              <div className="rounded-lg border border-brand-200 bg-brand-50/60 p-3 dark:border-brand-900/60 dark:bg-brand-950/20">
                <div className="font-medium text-slate-800 dark:text-zinc-100">通过 BitBrowser 接管原生凭据</div>
                <p className="mt-1 text-xs text-slate-500 dark:text-zinc-400">
                  助手会在选定的 BitBrowser 窗口打开 Trae 原生授权页，回调和 Token 交换自动完成；成功后可删除临时窗口。
                </p>
                <div className="mt-3 flex gap-2">
                  <select
                    value={bitWindowId}
                    onChange={(event) => setBitWindowId(event.target.value)}
                    className="input min-w-0 flex-1 text-xs"
                    disabled={bitLoading || bitBusy}
                  >
                    <option value="">选择 BitBrowser 窗口</option>
                    {bitProfiles.map((profile) => (
                      <option key={profile.id} value={profile.id}>
                        {profile.seq != null ? `窗口 ${profile.seq}` : '未编号'}
                        {profile.name ? ` · ${profile.name}` : ''}
                        {profile.open ? ' · 已打开' : ''}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    onClick={() => void loadBitProfiles()}
                    className="btn-ghost whitespace-nowrap text-xs"
                    disabled={bitLoading || bitBusy}
                  >
                    {bitLoading ? '读取中…' : '刷新窗口'}
                  </button>
                </div>
                <div className="mt-2 grid grid-cols-2 gap-2">
                  <input
                    value={accountName}
                    onChange={(event) => setAccountName(event.target.value)}
                    className="input text-xs"
                    placeholder="账号备注名（可选）"
                    disabled={bitBusy}
                  />
                  <select
                    value={gid}
                    onChange={(event) => setGid(event.target.value)}
                    className="input text-xs"
                    disabled={bitBusy}
                  >
                    <option value="">不分组</option>
                    {groups.map((group) => (
                      <option key={group.id} value={group.id}>
                        {group.name}
                      </option>
                    ))}
                  </select>
                </div>
                <button
                  type="button"
                  onClick={() => void startBitBrowserLogin()}
                  className="btn-primary mt-3 w-full justify-center"
                  disabled={bitBusy || bitLoading || !bitWindowId}
                >
                  {bitBusy ? '等待授权回调…' : '在 BitBrowser 中开始登录'}
                </button>
              </div>
            )}
            <div>
              点击「打开登录页」在系统浏览器中发起 OAuth 登录。若本机回环监听可用，完成授权后会自动回填；监听不可用时仍可手动粘贴回调 URL。
            </div>
            <div className="rounded-lg border border-amber-200 bg-amber-50/70 p-3 text-xs dark:border-amber-900/60 dark:bg-amber-950/20">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div>
                  <p className="font-medium text-amber-800 dark:text-amber-200">还没有 Trae 账号？</p>
                  <p className="mt-1 text-amber-700/80 dark:text-amber-300/80">
                    可在清洁/无痕浏览器中手动注册；这只隔离浏览器会话，不保证服务端不会绑定设备或触发风控。
                  </p>
                </div>
                <button
                  type="button"
                  className="btn-outline flex shrink-0 items-center gap-1 text-xs"
                  onClick={() => void openRegistrationPage()}
                  disabled={opening || busy || bitBusy}
                >
                  <ExternalLink size={13} />
                  打开注册入口
                </button>
              </div>
            </div>
          </div>
        )}

        {step === 2 && (
          <div>
            <label className="label">回调 URL</label>
            <textarea
              value={callbackUrl}
              onChange={(e) => setCallbackUrl(e.target.value)}
              className="input min-h-[100px] font-mono text-xs"
              placeholder="http://127.0.0.1:17388/authorize?code=..."
            />
            <p className="mt-2 text-xs text-slate-400">
              {autoWaiting
                ? '正在等待本机回环回调，完成浏览器授权后这里会自动跳到下一步；也可以直接粘贴地址。'
                : autoCallback
                  ? '已收到本机回环回调；如需重试，可重新打开登录页。'
                  : '登录完成后，浏览器会跳转到 http://127.0.0.1:17388/authorize?... 页面，请将地址栏完整 URL 复制粘贴到此处。'}
            </p>
          </div>
        )}

        {step === 3 && (
          <div className="space-y-3">
            <div>
              <label className="label">账号备注名（可选）</label>
              <input
                value={accountName}
                onChange={(e) => setAccountName(e.target.value)}
                className="input"
                placeholder="例如：me_1676"
              />
            </div>
            <div>
              <label className="label">分组（可选）</label>
              <select
                value={gid}
                onChange={(e) => setGid(e.target.value)}
                className="input"
              >
                <option value="">不分组</option>
                {groups.map((g) => (
                  <option key={g.id} value={g.id}>
                    {g.name}
                  </option>
                ))}
              </select>
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}

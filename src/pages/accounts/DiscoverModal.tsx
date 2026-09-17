import { CheckCircle2, Loader2, Plus, RefreshCw } from 'lucide-react';
import { Badge, Modal } from '../../components/ui';
import type { DiscoveredAccount } from '../../types';

/** F-08/F-75 自动发现弹框：展示本机 Trae 与 BitBrowser Trae Work 登录账号 */
export function DiscoverModal({
  open,
  scanning,
  discovered,
  addingUid,
  onClose,
  onAdd,
  onRescan,
}: {
  open: boolean;
  scanning: boolean;
  discovered: DiscoveredAccount[] | null;
  addingUid: string | null;
  onClose: () => void;
  onAdd: (d: DiscoveredAccount) => void;
  onRescan: () => void;
}) {
  const notInPool = discovered?.filter((d) => !d.in_pool).length ?? 0;
  return (
    <Modal open={open} onClose={onClose} title="扫描 Trae / BitBrowser 登录账号" size="lg">
      <div className="space-y-3 text-sm">
        <p className="text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
          本机 Trae Work / Trae 通过 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">storage.json</code> 与
          {' '}<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">state.vscdb</code> 识别登录账号；
          BitBrowser 则扫描备注/网址为 Trae Work 的 profile，通过 CDP 读取页面 localStorage 中的登录态。
          完整 JWT 只在本地加密 vault 内流转，不会展示、写日志或上传。
        </p>

        {scanning && (
          <div className="flex items-center gap-2 py-4 text-sm text-slate-500">
            <Loader2 size={15} className="animate-spin" /> 正在扫描本机应用与 BitBrowser…
          </div>
        )}

        {!scanning && discovered && discovered.length === 0 && (
          <div className="py-4 text-center text-sm text-slate-400">
            未发现已登录账号（本机应用未登录，或 BitBrowser 未运行/没有 Trae Work profile）
          </div>
        )}

        {!scanning && discovered && discovered.length > 0 && (
          <>
            {[
              { key: 'local', label: '本机 Trae 应用', list: discovered.filter((d) => d.source !== 'bitbrowser') },
              { key: 'bitbrowser', label: 'BitBrowser · Trae Work', list: discovered.filter((d) => d.source === 'bitbrowser') },
            ].map(({ key, label, list }) => {
              if (list.length === 0) return null;
              return (
                <div key={key} className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
                  <div className="mb-2 flex items-center justify-between">
                    <span className="font-semibold">{label}</span>
                    <span className="text-xs text-slate-400">{list.length} 个登录账号</span>
                  </div>
                  <div className="space-y-1.5">
                    {list.map((d) => (
                      <div
                        key={`${d.source ?? 'local'}-${d.app}-${d.user_id}-${d.window_id ?? d.storage_path}`}
                        className="flex items-center justify-between gap-2 rounded bg-slate-50 px-2 py-1.5 dark:bg-zinc-900"
                      >
                        <div className="min-w-0">
                          <div className="truncate font-mono text-xs">{d.user_id || '未读取到账号 UID'}</div>
                          {d.source === 'bitbrowser' && (
                            <div className="truncate text-[10px] text-slate-400 dark:text-zinc-500">
                              窗口{d.window_seq != null ? ` ${d.window_seq}` : ''}{d.window_name ? ` · ${d.window_name}` : ''}
                              {d.token_present ? ' · 已捕获登录态' : ' · 未发现登录态'}
                            </div>
                          )}
                          {d.source === 'bitbrowser' && d.read_error ? (
                            <div className="text-[10px] text-amber-600 dark:text-amber-400">{d.read_error}</div>
                          ) : d.uid_confident ? (
                            d.dc_uid ? (
                              <div className="truncate text-[10px] text-slate-400 dark:text-zinc-500">
                                账户中心 uid：{d.dc_uid}（与账号池 id 体系不同，仅作参考）
                              </div>
                            ) : null
                          ) : (
                            <div className="text-[10px] text-amber-600 dark:text-amber-400">
                              无法确认账号池 uid（仅识别到账户中心 id），暂不能入池
                            </div>
                          )}
                        </div>
                        {d.in_pool && d.source !== 'bitbrowser' ? (
                          <Badge tone="green">
                            <CheckCircle2 size={12} /> 已入池
                          </Badge>
                        ) : d.uid_confident && (d.source !== 'bitbrowser' || d.token_present) ? (
                          <button
                            onClick={() => onAdd(d)}
                            disabled={addingUid === d.user_id}
                            className="btn-outline !px-2 !py-1 text-xs disabled:cursor-not-allowed disabled:opacity-60"
                          >
                            {addingUid === d.user_id ? (
                              <>
                                <Loader2 size={12} className="animate-spin" /> 加入中
                              </>
                            ) : (
                              <>
                                <Plus size={12} /> {d.in_pool ? '更新接管' : '加入'}
                              </>
                            )}
                          </button>
                        ) : (
                          <span className="text-[10px] text-slate-400">不可入池</span>
                        )}
                      </div>
                    ))}
                  </div>
                </div>
              );
            })}
            <div className="flex items-center justify-between pt-1">
              <span className="text-xs text-slate-400">
                {notInPool > 0 ? `${notInPool} 个账号未入池` : '所有登录账号均已入池'}
              </span>
              <button onClick={onRescan} className="btn-outline text-xs">
                <RefreshCw size={12} /> 重新扫描
              </button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}

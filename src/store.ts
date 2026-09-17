import { create } from 'zustand';
import { APP_NAME } from './lib/about';
import { sendNotification } from '@tauri-apps/plugin-notification';
import { api, setupListeners, type AutoSwitchRequestEvent, type CheckinProgressEvent, type ProfileDoneEvent, type SaveLoginDoneEvent } from './lib/tauri';
import { withMinDelay } from './lib/delay';
import { copyTextToClipboard } from './lib/clipboard';
import type {
  AccountView,
  ApiServiceStatus,
  CheckinAccountResult,
  CheckinDone,
  CreditRecord,
  CreditsDailySnapshot,
  EnvStatus,
  GroupView,
  LocalEntitlement,
  LogLine,
  ProfileInfo,
  ProxyStatus,
  Settings,
  ViewKey,
  AppKey,
} from './types';
import { APP_HOME_VIEW } from './types';

export type ToastKind = 'info' | 'success' | 'error' | 'warn';
export interface Toast {
  id: number;
  kind: ToastKind;
  msg: string;
}

export interface CheckinRetryInfo {
  /** 第几轮重试（1 起） */
  round: number;
  /** 本轮重试的失败账号数 */
  total: number;
  /** 本轮开始时刻（Date.now() 毫秒），用于倒计时展示 */
  until: number;
}

export interface CheckinState {
  active: boolean;
  total: number;
  index: number;
  results: CheckinAccountResult[];
  done: CheckinDone | null;
  /** 失败自动重试状态（非 null 时展示重试横幅/倒计时，T5） */
  retry: CheckinRetryInfo | null;
}

export interface LogQuery {
  logType?: string;
  date?: string;
  keyword?: string;
  limit?: number;
}

interface AppState {
  ready: boolean;
  view: ViewKey;
  /** 侧边栏应用切换（trae = 现有菜单；buddy = 后期扩展置灰；doubao = 豆包页） */
  activeApp: AppKey;
  env: EnvStatus | null;
  envCn: EnvStatus | null;
  certInstalled: boolean;
  proxy: ProxyStatus;
  apiStatus: ApiServiceStatus | null;
  accounts: AccountView[];
  groups: GroupView[];
  settings: Settings | null;
  logs: LogLine[];
  creditsHistory: CreditRecord[];
  creditsDaily: CreditsDailySnapshot[];
  proxyLog: string[];
  switchProgress: string[];
  switchingTo: string | null;
  /** Trae Work CN 一键切号接力状态（保存项目元数据后再切换） */
  relayProgress: string[];
  relayActive: boolean;
  saveLoginProgress: string[];
  savingLogin: string | null;
  deviceResetProgress: string[];
  deviceResetActive: boolean;
  checkin: CheckinState;
  toasts: Toast[];
  profiles: ProfileInfo[];
  profileProgress: string[];
  profileActive: boolean;
  /** 快照管理当前查看的目标应用：TraeWork=TRAE SOLO CN / Trae=Trae CN IDE（F-03 参数化） */
  profileApp: 'TraeWork' | 'Trae';
  /** 本机两个 Trae 应用当前登录账号的套餐信息（storage.json 明文，零 API） */
  localEntitlement: LocalEntitlement | null;
  /** 全局 API 管理弹窗（unified-api-gateway-design §5.2；任意 activeApp 视图均可打开） */
  showApiManager: boolean;

  init: () => Promise<void>;
  setView: (v: ViewKey) => void;
  /** 切换侧边栏应用 Tab，并跳到该应用默认首页 */
  setActiveApp: (app: AppKey) => void;
  applyCheckinEvent: (e: CheckinProgressEvent) => void;

  refreshEnv: () => Promise<void>;
  refreshCert: () => Promise<void>;
  refreshProxy: () => Promise<void>;
  refreshApiStatus: () => Promise<void>;
  refreshAccounts: () => Promise<void>;
  refreshGroups: () => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshLogs: (q?: LogQuery, manual?: boolean) => Promise<void>;
  refreshCreditsHistory: () => Promise<void>;
  refreshCreditsDaily: () => Promise<void>;
  refreshProfiles: () => Promise<void>;
  setProfileApp: (app: 'TraeWork' | 'Trae') => Promise<void>;
  refreshLocalEntitlement: () => Promise<void>;
  setShowApiManager: (v: boolean) => void;

  startProxy: () => Promise<void>;
  stopProxy: () => Promise<void>;
  simulateAutoSwitch: () => Promise<void>;
  openTraeWithProxy: () => Promise<void>;
  openTraeCn: () => Promise<void>;
  addAccount: (name: string, jwt: string, groupId?: string) => Promise<void>;
  deleteAccount: (userId: string, deleteProfile: boolean) => Promise<void>;
  updateAccount: (userId: string, name?: string, jwt?: string) => Promise<void>;
  createGroup: (name: string, color: string) => Promise<void>;
  updateGroup: (
    id: string,
    patch: { name?: string; color?: string; order?: number },
  ) => Promise<void>;
  removeGroup: (id: string) => Promise<void>;
  moveAccount: (userId: string, groupId: string | null) => Promise<void>;
  resetDevice: (userId: string) => Promise<void>;
  switchTo: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy') => Promise<void>;
  switchAndContinue: (userId: string, targetApp?: 'TraeWork' | 'Trae') => Promise<void>;
  /** C1：一键以账号 X 打开豆包（恢复快照后拉起客户端；代理运行中时注入代理） */
  openDoubaoAs: (userId: string, proxyPort?: number) => Promise<void>;
  saveCurrentLogin: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy') => Promise<void>;
  /**
   * 续期账号登录态。
   * BitBrowser 账号按 UID 重新捕获，不依赖原 profile/window ID；旧版本没有
   * credential_source 标记的账号也会先尝试该路径，失败后再走本机快照。
   */
  renewJwt: (userId: string, credentialSource?: string | null) => Promise<void>;
  resetDeviceIds: (targetApp?: 'TraeWork' | 'Trae') => Promise<void>;
  startCheckin: (opts: {
    scope: string;
    user_ids?: string[];
    skip_checked_in: boolean;
    skip_expired: boolean;
  }) => Promise<void>;
  refreshRemainingCredits: (options?: { silent?: boolean }) => Promise<void>;
  cooldownClear: (userId: string) => Promise<void>;
  refreshJwt: (userId: string) => Promise<void>;
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
  profileBackup: (userId: string) => Promise<void>;
  profileRestore: (userId: string) => Promise<void>;
  profileDelete: (userId: string) => Promise<void>;
  oauthLogin: (callbackUrl: string, accountName?: string, groupId?: string) => Promise<void>;
  oauthLoginBitBrowser: (windowId: string, accountName?: string, groupId?: string) => Promise<void>;

  pushToast: (kind: ToastKind, msg: string) => void;
  dismissToast: (id: number) => void;
}

let toastSeq = 0;
// 已注册的事件监听取消函数；StrictMode 下 init 会执行两次，靠它先注销旧监听避免重复注册
let unsubs: Array<() => void> = [];
/** init 幂等锁：StrictMode 双跑（dev）时第二次调用直接返回，防并发双注册监听 */
let initStarted = false;
/** 全局积分刷新互斥：手动刷新、签到完成刷新和定时刷新共用同一条请求，避免并发打满接口。 */
let creditsRefreshInFlight: Promise<void> | null = null;
/** 自动切号事件可能在同一秒内由多个请求触发，前端也做一次互斥。 */
let desktopAutoSwitchInFlight = false;

function pickDesktopAutoSwitchTarget(
  accounts: AccountView[],
  currentUid: string | null,
  enabledUids?: Set<string>,
  preferWorkCredits = false,
): AccountView | null {
  const now = Math.floor(Date.now() / 1000);
  const candidates = accounts
    .filter((account) => account.user_id !== currentUid)
    .filter((account) => !enabledUids || enabledUids.has(account.user_id))
    .filter((account) => !(account.cooldown_until && account.cooldown_until > now))
    // 没有 refresh token 的过期登录态无法无感恢复，跳过；有 refresh token 的账号由
    // switch_account 在切换前自动刷新 JWT。
    .filter((account) => account.has_refresh_token || !account.jwt_exp_timestamp || account.jwt_exp_timestamp > now + 60);
  if (candidates.length === 0) return null;
  candidates.sort((a, b) => {
    const balanceA = (a.general_credits ?? 0) + (a.work_credits ?? 0);
    const balanceB = (b.general_credits ?? 0) + (b.work_credits ?? 0);
    const primaryA = preferWorkCredits ? (a.work_credits ?? 0) : (a.general_credits ?? 0);
    const primaryB = preferWorkCredits ? (b.work_credits ?? 0) : (b.general_credits ?? 0);
    // Seedance 消耗 Work 积分，文字对话优先通用积分；同类余额优先时仍以总余额
    // 作为第二排序键，兼容部分账号尚未刷新某一类积分的情况。
    if ((primaryA > 0) !== (primaryB > 0)) return primaryB > 0 ? 1 : -1;
    if (primaryA !== primaryB) return primaryB - primaryA;
    if (balanceA !== balanceB) return balanceB - balanceA;
    const expA = a.jwt_exp_timestamp ?? 0;
    const expB = b.jwt_exp_timestamp ?? 0;
    return expB - expA;
  });
  // 若所有缓存余额都是 0/未知，仍返回第一个可用账号，让服务端真实结果决定是否继续。
  return candidates[0];
}

function defaultSettings(): Settings {
  return {
    proxy_port: 8899,
    theme: 'system',
    launch_minimized: false,
    silent_checkin: false,
    auto_start_proxy: true,
    tray: true,
    language: 'zh-CN',
    checkin_skip_checked: true,
    checkin_skip_expired: true,
    retry: 1,
    notify: 'toast',
    trae_path: null,
    trae_cn_path: null,
    bitbrowser_api_url: 'http://127.0.0.1:54345',
    doubao_path: null,
    doubao_renew_url: 'https://www.doubao.com/info/v2/',
    doubao_quota_url: 'https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/',
    doubao_snapshot_include_idb: false,
    workbuddy_path: null,
    codebuddy_path: null,
    wb_auth_file_path: null,
    data_dir: null,
    log_retention_days: 30,
    proxy_domains: 'trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com',
    proxy_log_path: null,
    api_port: 7864,
    api_default_model: 'deepseek-v4-flash',
    desktop_auto_switch: false,
    desktop_auto_switch_cooldown_secs: 60,
  };
}

// 日志轮询去重：上一轮未返回时跳过本轮（防 2s 轮询堆积与旧响应乱序覆盖）
let logsPollInflight = false;
// 日志查询并发序号（最新请求胜出）：手动刷新与轮询并发时，旧响应直接丢弃
let logsReqSeq = 0;
// 同因 toast 限频：读取持续失败期间每 60s 最多弹一次（防 toast 风暴）；手动调用直通
let lastLogsErrToastAt = 0;

/** 轮询等待后端事件把切换/保存状态置回空值；事件缺失时有明确超时返回。 */
async function waitForStore(
  predicate: () => boolean,
  timeoutMs = 120_000,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) return false;
    await new Promise<void>((resolve) => setTimeout(resolve, 250));
  }
  return true;
}

export const useAppStore = create<AppState>((set, get) => ({
  ready: false,
  view: 'dashboard',
  activeApp: 'trae',
  env: null,
  envCn: null,
  certInstalled: false,
  proxy: { running: false, port: 0, captured: 0, started_at: null },
  apiStatus: null,
  accounts: [],
  groups: [],
  settings: null,
  logs: [],
  creditsHistory: [],
  creditsDaily: [],
  proxyLog: [],
  switchProgress: [],
  switchingTo: null,
  relayProgress: [],
  relayActive: false,
  saveLoginProgress: [],
  savingLogin: null,
  deviceResetProgress: [],
  deviceResetActive: false,
  checkin: { active: false, total: 0, index: 0, results: [], done: null, retry: null },
  toasts: [],
  profiles: [],
  profileProgress: [],
  profileActive: false,
  profileApp: 'TraeWork',
  localEntitlement: null,
  showApiManager: false,

  init: async () => {
    // StrictMode 下 effect 会执行两次（dev）：两次 init 同步并发启动，旧的「先注销旧监听」
    // 防护在 setupListeners resolve 前执行时注销不到任何东西，两组监听都会注册成功，
    // 且后者覆盖 unsubs → 第一组永久泄漏、事件双发（proxy-log 重复、captured +2）。
    // 幂等锁：仅首次执行注册，后续调用直接复用（审查修复 P1-16）
    if (initStarted) return;
    initStarted = true;
    unsubs = await setupListeners({
      onProxyLog: (line) =>
        set((s) => ({ proxyLog: [line, ...s.proxyLog.slice(0, 199)] })),
      onAutoSwitchRequest: async (event: AutoSwitchRequestEvent) => {
        const current = get();
        const reasonText = event.reason === 'auth' ? '鉴权失效' : '额度不足';
        set((s) => ({
          relayProgress: [
            ...s.relayProgress.slice(-49),
            `[监控] 收到${reasonText}信号（HTTP ${event.status || 0}），准备检查候选账号…`,
          ],
        }));
        // 本地模拟事件不依赖真实自动切号开关；这样用户可以在启用前先验证
        // 候选账号选择链路。真实代理事件仍严格要求开关已保存为启用。
        if (!current.settings?.desktop_auto_switch && !event.simulated) {
          set((s) => ({ relayProgress: [...s.relayProgress.slice(-49), '[监控] 自动切换开关未启用，已停止'] }));
          return;
        }
        if (desktopAutoSwitchInFlight) {
          set((s) => ({ relayProgress: [...s.relayProgress.slice(-49), '[监控] 已有自动切换任务进行中，忽略重复信号'] }));
          return;
        }
        if (current.relayActive || current.switchingTo || current.savingLogin) {
          current.pushToast('warn', '检测到额度/鉴权异常，但当前已有切换任务，已跳过自动切换');
          set((s) => ({ relayProgress: [...s.relayProgress.slice(-49), '[监控] 当前已有切换/接力任务，已跳过'] }));
          return;
        }
        desktopAutoSwitchInFlight = true;
        try {
          let accounts = get().accounts;
          if (accounts.length === 0) {
            await get().refreshAccounts();
            accounts = get().accounts;
          }
          // 若 API 账号池已配置，自动切换严格遵循其中的 enabled_uids；池为空时
          // 退化为账号库全量，避免用户尚未配置网关池时完全无法切号。
          let enabledUids: Set<string> | undefined;
          try {
            const pool = await api.apiServer.poolList();
            if (pool.enabled_uids.length > 0) enabledUids = new Set(pool.enabled_uids);
          } catch {
            /* API 服务未启用时，使用账号库全量候选 */
          }
          const target = pickDesktopAutoSwitchTarget(
            accounts,
            event.currentUid,
            enabledUids,
            event.path.toLowerCase().includes('/api/ide/v1/tool_text_to_video_stream'),
          );
          if (!target) {
            set((s) => ({ relayProgress: [...s.relayProgress.slice(-49), '[失败] 自动切换未找到可用候选账号'] }));
            get().pushToast('error', '自动切换未找到可用账号，请检查账号池或先刷新积分');
            return;
          }
          if (event.simulated) {
            const reasonText = event.reason === 'auth' ? '鉴权失效' : '额度不足';
            set((s) => ({
              relayProgress: [
                ...s.relayProgress.slice(-49),
                `[模拟] 检测到${reasonText}，候选账号：${target.user_id}（未执行切换）`,
              ],
            }));
            get().pushToast('success', `模拟成功：将选择账号 ${target.user_id}，未执行切换、未消耗积分`);
            return;
          }
          get().pushToast('info', `检测到${reasonText}，正在切换到账号 ${target.user_id}…`);
          await get().switchAndContinue(target.user_id, 'TraeWork');
        } finally {
          desktopAutoSwitchInFlight = false;
        }
      },
      onAccountCaptured: (uid) => {
        // 事件驱动累加捕获数（后端 Arc<AtomicI64> 的实时镜像，避免轮询）
        set((s) => ({ proxy: { ...s.proxy, captured: s.proxy.captured + 1 } }));
        get().pushToast('success', `已捕获账号 ${uid}`);
        void get().refreshAccounts();
      },
      onCheckinProgress: (e) => get().applyCheckinEvent(e),
      onSwitchProgress: (line) =>
        set((s) => ({ switchProgress: [...s.switchProgress.slice(-49), line] })),
      // D2：订阅后端 switch-done，给用户明确的切换完成/失败信号
      onSwitchDone: (e) => {
        // 提取脚本 [fatal] 行的原文（如「目标账号 xxx 无快照，请先登录该账号并点击保存当前登录态」）
        // raw 兜底空串：后端偶发缺 payload 时避免 TypeError 把 switchingTo 永久锁死
        const raw = e.raw ?? '';
        const reason = e.success ? null : (raw.match(/\[fatal\]\s*(.+)$/)?.[1]?.trim() ?? null);
        // verify 超时（authfile 布局）：切换动作完成但登录身份未确认 → warn 而非 success，
        // 避免「切换成功」toast 掩盖客户端未登录的事实（switcher.log 实测 4/4 超时）
        const warnUnconfirmed = e.success && /未确认登录身份/.test(raw);
        const doneLine = warnUnconfirmed
          ? '[警告] 已切换，但 30 秒内未确认登录身份，请打开客户端核实'
          : e.success
            ? '[完成] 登录态切换成功'
            : `[失败] ${reason ?? '登录态切换未完成，请查看日志'}`;
        set((s) => ({
          switchingTo: null,
          switchProgress: [...s.switchProgress.slice(-49), doneLine],
        }));
        get().pushToast(
          warnUnconfirmed ? 'warn' : e.success ? 'success' : 'error',
          warnUnconfirmed
            ? '已执行切换，但 30 秒内未确认登录身份——请打开客户端核实；若未登录，请重新登录后「保存当前登录态」'
            : e.success
              ? '登录态切换完成'
              : `切换失败：${reason ?? '请查看系统日志'}`,
        );
        void get().refreshAccounts();
        void get().refreshProxy();
      },
      onSaveLoginProgress: (line) =>
        set((s) => ({ saveLoginProgress: [...s.saveLoginProgress.slice(-49), line] })),
      onSaveLoginDone: (e: SaveLoginDoneEvent) => {
        // 同上：raw 缺失时兜底空串，避免 savingLogin 被锁死
        const reason = e.success ? null : ((e.raw ?? '').match(/\[fatal\]\s*(.+)$/)?.[1]?.trim() ?? null);
        set((s) => ({
          savingLogin: null,
          saveLoginProgress: [
            ...s.saveLoginProgress.slice(-49),
            e.success ? '[完成] 登录态保存成功' : `[失败] ${reason ?? '登录态保存失败，请查看日志'}`,
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success ? '登录态已保存，可随时切换回此账号' : `保存失败：${reason ?? '请查看系统日志'}`,
        );
        void get().refreshProfiles();
      },
      onDeviceResetProgress: (line) =>
        set((s) => ({ deviceResetProgress: [...s.deviceResetProgress.slice(-99), line] })),
      onDeviceResetDone: (e) => {
        set((s) => ({
          deviceResetActive: false,
          deviceResetProgress: [
            ...s.deviceResetProgress.slice(-99),
            e.success ? '[完成] 6 层设备标识重置成功' : '[失败] 设备标识重置未完成，请查看日志',
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success ? '6 层设备标识重置完成' : '设备标识重置失败，请查看日志',
        );
      },
      onProfileProgress: (line) =>
        set((s) => ({ profileProgress: [...s.profileProgress.slice(-49), line] })),
      onProfileDone: (e: ProfileDoneEvent) => {
        set((s) => ({
          profileActive: false,
          profileProgress: [
            ...s.profileProgress.slice(-49),
            e.success ? `[完成] ${e.action === 'backup' ? '备份' : '恢复'}成功` : `[失败] ${e.action === 'backup' ? '备份' : '恢复'}失败`,
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success
            ? `登录态${e.action === 'backup' ? '备份' : '恢复'}完成`
            : `登录态${e.action === 'backup' ? '备份' : '恢复'}失败`,
        );
        void get().refreshProfiles();
      },
    });
    await Promise.all([
      get().refreshEnv(),
      get().refreshCert(),
      get().refreshProxy(),
      get().refreshApiStatus(),
      get().refreshAccounts(),
      get().refreshGroups(),
      get().refreshSettings(),
      get().refreshCreditsHistory(),
      get().refreshCreditsDaily(),
      get().refreshProfiles(),
      get().refreshLocalEntitlement(),
    ]);
    set({ ready: true });

    // 启动时根据设置自动开启代理
    const s = get();
    if (!s.proxy.running && s.settings?.auto_start_proxy) {
      void s.startProxy();
    }
  },

  setView: (v) => set({ view: v }),
  setActiveApp: (app) => set({ activeApp: app, view: APP_HOME_VIEW[app] }),
  setShowApiManager: (v) => set({ showApiManager: v }),

  applyCheckinEvent: (e) => {
    set((s) => {
      if (e.type === 'start') {
        // Rust 侧 start 带 scope 内全集清单（候选 pending / 跳过带原因 / 重试轮沿用上轮状态），
        // 重建列表使被跳过的账号也可见；Python 转发的 start（无 accounts）仅同步候选总数
        if (e.accounts) {
          return {
            checkin: {
              active: true,
              total: e.total,
              index: 0,
              results: e.accounts.map((a, i) => ({
                index: i + 1,
                user_id: a.user_id,
                name: a.name,
                status: a.status,
                skip_reason: a.skip_reason ?? null,
              })),
              done: null,
              retry: s.checkin.retry,
            },
          };
        }
        // Python 转发的 start（无 accounts）同样重置进度与旧结果，避免上一轮残留干扰本轮展示
        return { checkin: { ...s.checkin, active: true, total: e.total, index: 0, results: [], done: null } };
      }
      if (e.type === 'retry') {
        return {
          checkin: {
            ...s.checkin,
            active: true,
            retry: { round: e.round, total: e.total, until: Date.now() + e.delay * 1000 },
          },
        };
      }
      if (e.type === 'account') {
        const results = s.checkin.results.slice();
        // 按 user_id 匹配行：事件 index 是本轮候选内的序号，与全集列表位置无关
        const i = results.findIndex((r) => r.user_id === e.user_id);
        const row = {
          index: i >= 0 ? results[i].index : results.length + 1,
          user_id: e.user_id,
          name: e.name,
          status: e.status,
          skip_reason: null,
          credits: e.credits,
          delta: e.delta,
          elapsed: e.elapsed,
          code: e.code,
          message: e.message,
          error_type: e.error_type,
          cooldown_until: e.cooldown_until,
        };
        if (i >= 0) results[i] = row;
        else results.push(row);
        // index 累计本轮已处理候选数，驱动进度条（total 口径=本轮候选数）
        return { checkin: { ...s.checkin, index: s.checkin.index + 1, results } };
      }
      return {
        checkin: {
          ...s.checkin,
          active: false,
          retry: null,
          done: { ok: e.ok, already: e.already, failed: e.failed, total: e.total },
        },
      };
    });
    if (e.type === 'done') {
      void get().refreshAccounts();
      // JWT 吊销类失败的精确提示（issue #9）：401=服务端已吊销 JWT，重新登录+保存即可恢复，
      // 不再让用户面对笼统的「失败 N」自己摸索原因
      const deadCount = get().checkin.results.filter(
        (r) => r?.status === 'fail' && r.error_type === 'SessionDead',
      ).length;
      // 签到完成后静默刷新剩余积分（内部会再次 refreshAccounts）；走统一包装以复用
      // 定时器/手动刷新中的请求，避免签到结束时并发打接口。
      void get().refreshRemainingCredits({ silent: true }).then(() => {
        get().refreshCreditsDaily();
      });
      get().pushToast(
        e.failed > 0 ? 'warn' : 'success',
        // 空轮次：过滤后无候选（全部已签/过期/冷却中），给用户明确文案而非「成功 0 已签 0 失败 0」
        e.empty
          ? '没有需要签到的账号（全部已签/过期/冷却中）'
          : `签到完成：成功 ${e.ok}，已签 ${e.already}，失败 ${e.failed}`,
      );
      if (deadCount > 0) {
        get().pushToast(
          'error',
          `${deadCount} 个账号 JWT 已被服务端吊销（该账号在别处重新登录/IDE 内退出过登录）：请在 TRAE 中重新登录该账号并「保存当前登录态」，再点「续期 JWT」重新捕获`,
        );
      }
    }
  },

  refreshEnv: async () => {
    try {
      const env = await api.env.check();
      set({ env });
    } catch (err) {
      get().pushToast('error', `环境检测失败：${String(err)}`);
    }
    // Trae CN IDE 环境检测（独立应用，失败不影响主检测）
    try {
      const envCn = await api.env.checkCn();
      set({ envCn });
    } catch {
      set({ envCn: null });
    }
  },
  refreshCert: async () => {
    try {
      const r = await api.cert.status();
      set({ certInstalled: r.installed });
    } catch {
      /* ignore */
    }
  },
  refreshProxy: async () => {
    try {
      const proxy = await api.proxy.status();
      set({ proxy });
    } catch {
      /* ignore */
    }
  },
  refreshApiStatus: async () => {
    try {
      const s = await api.apiServer.status();
      set({ apiStatus: s });
    } catch {
      set({ apiStatus: null });
    }
  },
  refreshAccounts: async () => {
    try {
      const accounts = await api.accounts.list();
      set({ accounts });
    } catch (err) {
      get().pushToast('error', `读取账号失败：${String(err)}`);
    }
  },
  refreshGroups: async () => {
    try {
      const groups = await api.groups.list();
      set({ groups });
    } catch {
      /* ignore */
    }
  },
  refreshSettings: async () => {
    try {
      const settings = await api.misc.settingsGet();
      set({ settings });
    } catch {
      set({ settings: defaultSettings() });
    }
  },
  refreshLogs: async (q, manual) => {
    if (logsPollInflight && !manual) return;
    logsPollInflight = true;
    // 最新请求胜出：手动刷新绕过 inflight 防护会与轮询并发，旧响应不得覆盖新数据
    const seq = ++logsReqSeq;
    try {
      const logs = await api.misc.logsQuery({
        logType: q?.logType,
        date: q?.date,
        keyword: q?.keyword,
        limit: q?.limit ?? 500,
      });
      if (seq !== logsReqSeq) return;
      set({ logs });
    } catch (err) {
      if (seq !== logsReqSeq) return;
      const now = Date.now();
      if (manual || now - lastLogsErrToastAt > 60_000) {
        lastLogsErrToastAt = now;
        get().pushToast('error', `读取日志失败：${String(err)}`);
      }
    } finally {
      logsPollInflight = false;
    }
  },
  refreshCreditsHistory: async () => {
    try {
      const creditsHistory = await api.misc.creditsHistory();
      set({ creditsHistory });
    } catch (err) {
      get().pushToast('error', `读取积分历史失败：${String(err)}`);
    }
  },
  refreshCreditsDaily: async () => {
    try {
      const creditsDaily = await api.accounts.dailyList();
      set({ creditsDaily });
    } catch {
      /* ignore */
    }
  },

  startProxy: async () => {
    const port = get().settings?.proxy_port || 8899;
    try {
      const proxy = await api.proxy.start(port);
      set({ proxy, proxyLog: [] });
      get().pushToast('success', `代理已启动（端口 ${proxy.port}）`);
    } catch (err) {
      get().pushToast('error', `启动代理失败：${String(err)}`);
    }
  },
  stopProxy: async () => {
    try {
      const proxy = await api.proxy.stop();
      set({ proxy });
      get().pushToast('info', '代理已停止');
    } catch (err) {
      get().pushToast('error', `停止代理失败：${String(err)}`);
    }
  },
  simulateAutoSwitch: async () => {
    set((s) => ({
      relayProgress: [
        ...s.relayProgress.slice(-49),
        '[模拟] 已发起自动切换链路检查…',
      ],
    }));
    try {
      await api.proxy.simulateAutoSwitch();
    } catch (err) {
      const message = `模拟自动切换失败：${String(err)}`;
      set((s) => ({
        relayProgress: [...s.relayProgress.slice(-49), `[模拟失败] ${String(err)}`],
      }));
      get().pushToast('error', message);
    }
  },
  openTraeWithProxy: async () => {
    const env = get().env;
    if (!env?.installed) {
      get().pushToast('info', '未检测到 Trae Work 安装，正在打开下载页…');
      try {
        await api.env.openSite();
      } catch (err) {
        get().pushToast('error', `打开下载页失败：${String(err)}`);
      }
      return;
    }
    // 确保代理在请求路径上：未运行则先启动，否则打开 Trae Work 也不会走代理、无法捕获账号
    let port: number | undefined = get().proxy.running ? get().proxy.port : undefined;
    if (!port) {
      // 可能残留端口为 0 的无效代理，先停掉再以有效端口重启
      if (get().proxy.running) {
        try { await get().stopProxy(); } catch { /* ignore */ }
      }
      get().pushToast('info', '正在启动代理以确保 Trae Work 走本地代理…');
      await get().startProxy();
      port = get().proxy.running ? get().proxy.port : undefined;
    }
    try {
      if (port) {
        await api.env.openApp(port);
        get().pushToast('success', `已打开 Trae Work（代理已注入 127.0.0.1:${port}）`);
      } else {
        // 代理启动失败：仍打开客户端，但明确告知不会捕获账号
        await api.env.openApp(undefined);
        get().pushToast('warn', '代理启动失败，已直接打开 Trae Work（账号不会被自动捕获）');
      }
    } catch (err) {
      get().pushToast('error', `打开 Trae Work 失败：${String(err)}`);
    }
  },
  openTraeCn: async () => {
    // 与 openTraeWithProxy 同款逻辑：先确保代理在运行并注入，Trae 的流量才走本地 MITM 代理
    let port: number | undefined = get().proxy.running ? get().proxy.port : undefined;
    if (!port) {
      // 可能残留端口为 0 的无效代理，先停掉再以有效端口重启
      if (get().proxy.running) {
        try { await get().stopProxy(); } catch { /* ignore */ }
      }
      get().pushToast('info', '正在启动代理以确保 Trae 走本地代理…');
      await get().startProxy();
      port = get().proxy.running ? get().proxy.port : undefined;
    }
    try {
      if (port) {
        await api.env.openCnApp(port);
        get().pushToast('success', `已打开 Trae（代理已注入 127.0.0.1:${port}）`);
      } else {
        // 代理启动失败：仍打开客户端，但明确告知不走代理
        await api.env.openCnApp(undefined);
        get().pushToast('warn', '代理启动失败，已直接打开 Trae（流量不会经过本地代理）');
      }
    } catch (err) {
      get().pushToast('error', `打开 Trae 失败：${String(err)}`);
    }
  },
  addAccount: async (name, jwt, groupId) => {
    try {
      await api.accounts.addManual(name, jwt, groupId);
      await get().refreshAccounts();
      get().pushToast('success', `账号「${name}」已添加`);
    } catch (err) {
      get().pushToast('error', `添加失败：${String(err)}`);
      throw err;
    }
  },
  deleteAccount: async (userId, deleteProfile) => {
    try {
      await api.accounts.delete(userId, deleteProfile);
      await get().refreshAccounts();
      get().pushToast('info', '账号已删除');
    } catch (err) {
      get().pushToast('error', `删除失败：${String(err)}`);
    }
  },
  updateAccount: async (userId, name, jwt) => {
    try {
      await api.accounts.update(userId, name, jwt);
      await get().refreshAccounts();
      get().pushToast('success', '账号已更新');
    } catch (err) {
      get().pushToast('error', `更新失败：${String(err)}`);
      throw err;
    }
  },
  createGroup: async (name, color) => {
    try {
      await api.groups.create(name, color);
      await get().refreshGroups();
      get().pushToast('success', `分组「${name}」已创建`);
    } catch (err) {
      get().pushToast('error', `创建分组失败：${String(err)}`);
    }
  },
  updateGroup: async (id, patch) => {
    try {
      await api.groups.update(id, patch);
      await get().refreshGroups();
    } catch (err) {
      get().pushToast('error', `更新分组失败：${String(err)}`);
    }
  },
  removeGroup: async (id) => {
    try {
      await api.groups.remove(id);
      await get().refreshGroups();
      await get().refreshAccounts();
      get().pushToast('info', '分组已删除');
    } catch (err) {
      get().pushToast('error', `删除分组失败：${String(err)}`);
    }
  },
  moveAccount: async (userId, groupId) => {
    try {
      await api.groups.move(userId, groupId);
      await get().refreshAccounts();
    } catch (err) {
      get().pushToast('error', `移动分组失败：${String(err)}`);
    }
  },
  resetDevice: async (userId) => {
    try {
      await api.misc.deviceReset(userId);
      await get().refreshAccounts();
      get().pushToast('success', '设备 ID 已重置');
    } catch (err) {
      get().pushToast('error', `重置失败：${String(err)}`);
    }
  },
  switchTo: async (userId, targetApp) => {
    try {
      set({ switchingTo: userId, switchProgress: [] });
      // withMinDelay：切换是高风险操作，保证 busy 态至少可见 1s（避免瞬间完成导致闪烁/误触连点）
      await withMinDelay(api.switchAccount(userId, targetApp));
      // 应用名后缀：TraeWork 静默；其余应用标注目标（Trae→Trae、WorkBuddy→WorkBuddy、Doubao→Doubao、CodeBuddy→CodeBuddy）
      get().pushToast('info', `正在切换登录态${targetApp && targetApp !== 'TraeWork' ? `（${targetApp === 'CodeBuddy' ? 'CodeBuddy' : targetApp}）` : ''}，请稍候…`);
    } catch (err) {
      set({ switchingTo: null });
      get().pushToast('error', `切换失败：${String(err)}`);
    }
  },
  switchAndContinue: async (userId, targetApp = 'TraeWork') => {
    if (get().relayActive || get().switchingTo || get().savingLogin) {
      get().pushToast('warn', '已有切换/保存任务进行中，请稍候');
      return;
    }
    set({ relayActive: true, relayProgress: ['[开始] 正在读取当前 Trae 项目…'] });
    let rollbackUid: string | null = null;
    try {
      const relay = await api.traeRelay.capture(targetApp);
      const project = relay.project_paths?.[0] ?? null;
      const sourceUid = relay.source_uid;
      if (!sourceUid) {
        throw new Error('未识别当前 Trae 登录账号，已停止接力以避免覆盖错误快照');
      }
      if (sourceUid === userId) {
        throw new Error('目标账号就是当前账号，无需切换');
      }
      set((s) => ({
        relayProgress: [
          ...s.relayProgress,
          `[完成] 当前账号 ${sourceUid}`,
          project ? `[完成] 将在切号后重开项目：${project}` : '[警告] 未找到最近项目，切号后不会自动重开项目',
        ],
      }));

      // 先确认网关没有生成请求，避免账号快照在流式响应中途被替换。
      await api.apiServer.waitIdle(120_000);
      // 切号会替换 Trae 的登录态；先按 Trae 原生协议生成分享包，确保新账号
      // 可以从同一份文本/图片/视频上下文继续。分享失败时中止切换，避免上下文丢失。
      set((s) => ({ relayProgress: [...s.relayProgress, '[进行中] 正在创建当前会话分享链接（会上传会话及媒体）…'] }));
      const share = await api.traeRelay.createShare(relay.session_id, targetApp);
      set((s) => ({
        relayProgress: [
          ...s.relayProgress,
          `[完成] 分享链接已生成（${share.resource_count} 个媒体资源，状态：${share.status}）`,
          '[完成] 分享地址已保存到本地接力包',
        ],
      }));
      set((s) => ({ relayProgress: [...s.relayProgress, '[进行中] 等待生成结束，保存当前登录态…'], savingLogin: sourceUid, saveLoginProgress: [] }));
      await api.saveCurrentLogin(sourceUid, targetApp);
      const saved = await waitForStore(() => !useAppStore.getState().savingLogin);
      const saveLine = get().saveLoginProgress[get().saveLoginProgress.length - 1] ?? '';
      if (!saved || saveLine.startsWith('[失败]')) {
        throw new Error(saveLine.replace(/^\[失败\]\s*/, '') || '保存当前登录态超时');
      }

      rollbackUid = sourceUid;
      set({ switchingTo: userId, switchProgress: [] });
      await withMinDelay(api.switchAccount(userId, targetApp));
      set((s) => ({ relayProgress: [...s.relayProgress, `[进行中] 正在切换到账号 ${userId}…`] }));
      const switched = await waitForStore(() => !useAppStore.getState().switchingTo);
      const switchLine = get().switchProgress[get().switchProgress.length - 1] ?? '';
      if (!switched || switchLine.startsWith('[失败]')) {
        throw new Error(switchLine.replace(/^\[失败\]\s*/, '') || '登录态切换超时');
      }

      if (project) {
        const proxyPort = get().proxy.running ? get().proxy.port : undefined;
        await api.traeRelay.openProject(project, targetApp, proxyPort);
        set((s) => ({ relayProgress: [...s.relayProgress, `[完成] 已重新打开项目：${project}`] }));
      } else {
        set((s) => ({ relayProgress: [...s.relayProgress, '[完成] 已切换账号；未自动重开项目'] }));
      }
      // 分享链接不携带账号凭证。切号后把链接放入系统剪贴板，用户可直接
      // 在 Trae Work 的新对话输入框粘贴；这比打开公开分享页再寻找“继续使用”
      // 更符合原生接力流程。原进行中的任务不会被伪造续接。
      try {
        await copyTextToClipboard(share.share_url);
        set((s) => ({ relayProgress: [...s.relayProgress, '[完成] 分享链接已复制，可在 Trae Work 新对话框直接粘贴'] }));
      } catch (clipboardError) {
        // 剪贴板不可用时保留原生分享页作为可恢复兜底，并明确告诉用户发生了降级。
        await api.traeRelay.openShare(share.share_url);
        set((s) => ({ relayProgress: [...s.relayProgress, `[警告] 无法写入系统剪贴板（${String(clipboardError)}），已打开分享页面` ] }));
      }
      set({ relayActive: false });
      get().pushToast('success', project ? '已切换账号、重开项目并复制会话链接' : '已切换账号并复制会话链接');
      void get().refreshAccounts();
    } catch (err) {
      const message = String(err);
      set((s) => ({ relayActive: false, savingLogin: null, switchingTo: null, relayProgress: [...s.relayProgress, `[失败] ${message}`] }));
      // 目标槽恢复失败可能留下半切换状态；在已保存当前态的前提下尝试一次回滚，
      // 避免用户停留在未知登录态。回滚失败也只报告，不覆盖原始错误。
      if (rollbackUid) {
        try {
          set({ switchingTo: rollbackUid, switchProgress: [] });
          await withMinDelay(api.switchAccount(rollbackUid, targetApp));
          const rolledBack = await waitForStore(() => !useAppStore.getState().switchingTo, 60_000);
          if (rolledBack) {
            set((s) => ({ relayProgress: [...s.relayProgress, `[回滚] 已恢复原账号 ${rollbackUid}`] }));
          } else {
            set((s) => ({ relayProgress: [...s.relayProgress, '[回滚] 未在时限内完成，请打开 Trae Work 核实登录态'] }));
          }
        } catch (rollbackError) {
          set((s) => ({ relayProgress: [...s.relayProgress, `[回滚失败] ${String(rollbackError)}`] }));
        } finally {
          set({ switchingTo: null });
        }
      }
      set({ relayActive: false, savingLogin: null });
      get().pushToast('error', `切换并继续失败：${message}${rollbackUid ? '（已尝试回滚）' : ''}`);
    }
  },
  saveCurrentLogin: async (userId, targetApp) => {
    try {
      set({ savingLogin: userId, saveLoginProgress: [] });
      await api.saveCurrentLogin(userId, targetApp);
      get().pushToast('info', '正在保存当前登录态，请稍候…');
    } catch (err) {
      set({ savingLogin: null });
      get().pushToast('error', `保存登录态失败：${String(err)}`);
    }
  },
  openDoubaoAs: async (userId, proxyPort) => {
    try {
      set({ switchingTo: userId, switchProgress: [] });
      await withMinDelay(api.doubao.openAs(userId, proxyPort));
      get().pushToast('info', `正在恢复账号 ${userId} 的快照并启动豆包，请稍候…`);
    } catch (err) {
      set({ switchingTo: null });
      get().pushToast('error', `以账号打开失败：${String(err)}`);
    }
  },
  renewJwt: async (userId, credentialSource) => {
    try {
      // BitBrowser 只负责首次注册/登录和一次性接管 refresh_token。
      // 后续续期统一走本机 Trae ExchangeToken，不再依赖 BitBrowser 窗口是否存在。
      get().pushToast('info', '正在通过原生 Trae Work CN 凭据续期 JWT…');
      await api.accounts.refreshJwt(userId);
      await get().refreshAccounts();
      get().pushToast('success', 'JWT 已续期，原生切换快照已同步');
    } catch (err) {
      const detail = String(err);
      const hint = credentialSource === 'bitbrowser'
        ? '；该账号尚未接管原生 refresh_token，请在 BitBrowser 中重新导入一次完成首次接管'
        : '';
      get().pushToast('error', `原生 JWT 续期失败：${detail}${hint}`);
    }
  },
  resetDeviceIds: async (targetApp) => {
    set({ deviceResetActive: true, deviceResetProgress: [] });
    try {
      await api.resetDeviceIds(targetApp);
      get().pushToast('info', `正在执行 6 层设备标识重置（${targetApp === 'Trae' ? 'Trae' : 'Trae Work'}）…`);
    } catch (err) {
      set({ deviceResetActive: false });
      get().pushToast('error', `设备标识重置失败：${String(err)}`);
    }
  },
  startCheckin: async (opts) => {
    // 重置签到状态，避免显示上一次的进度
    set({ checkin: { active: true, total: 0, index: 0, results: [], done: null, retry: null } });
    try {
      await api.checkin.start(opts);
    } catch (err) {
      set((s) => ({ checkin: { ...s.checkin, active: false } }));
      get().pushToast('error', `发起签到失败：${String(err)}`);
    }
  },
  refreshRemainingCredits: async (options = {}) => {
    // 定时器与手动刷新可能同时触发；后来的调用等待当前请求，不再重复请求全部账号。
    if (creditsRefreshInFlight) {
      await creditsRefreshInFlight;
      return;
    }

    const run = (async () => {
      try {
        const ok = await api.accounts.refreshRemainingCredits();
        await get().refreshAccounts();
        if (!options.silent && ok > 0) {
          get().pushToast('success', `已刷新 ${ok} 个账号的可用积分`);
        }
      } catch (err) {
        if (!options.silent) {
          get().pushToast('error', `刷新可用积分失败：${String(err)}`);
        }
      }
    })();
    creditsRefreshInFlight = run;
    try {
      await run;
    } finally {
      if (creditsRefreshInFlight === run) creditsRefreshInFlight = null;
    }
  },
  cooldownClear: async (userId) => {
    try {
      await api.accounts.cooldownClear(userId);
      await get().refreshAccounts();
      get().pushToast('success', '已解除冷却');
    } catch (err) {
      get().pushToast('error', `解除冷却失败：${String(err)}`);
    }
  },
  refreshJwt: async (userId) => {
    try {
      await api.accounts.refreshJwt(userId);
      await get().refreshAccounts();
      get().pushToast('success', 'JWT 已自动刷新');
    } catch (err) {
      get().pushToast('error', `JWT 刷新失败：${String(err)}`);
    }
  },
  saveSettings: async (patch) => {
    const current = get().settings ?? defaultSettings();
    const next = { ...current, ...patch } as Settings;
    set({ settings: next });
    try {
      await api.misc.settingsSet(next);
    } catch (err) {
      // 回滚到修改前的值，避免 UI 显示与后端不一致
      set({ settings: current });
      get().pushToast('error', `保存设置失败：${String(err)}`);
      // rethrow：让调用方 catch 感知失败，避免误弹「已保存」成功提示
      throw err;
    }
  },

  refreshProfiles: async () => {
    try {
      const profiles = await api.profiles.list(get().profileApp);
      set({ profiles });
    } catch {
      /* ignore */
    }
  },
  setProfileApp: async (app) => {
    set({ profileApp: app, profiles: [] });
    await get().refreshProfiles();
  },
  refreshLocalEntitlement: async () => {
    try {
      const localEntitlement = await api.traeApps.localEntitlement();
      set({ localEntitlement });
    } catch {
      /* ignore */
    }
  },
  profileBackup: async (userId) => {
    set({ profileActive: true, profileProgress: [] });
    try {
      await api.profiles.backup(userId, get().profileApp);
      get().pushToast('info', '正在备份登录态快照…');
    } catch (err) {
      set({ profileActive: false });
      get().pushToast('error', `备份失败：${String(err)}`);
    }
  },
  profileRestore: async (userId) => {
    set({ profileActive: true, profileProgress: [] });
    try {
      await api.profiles.restore(userId, get().profileApp);
      get().pushToast('info', '正在恢复登录态快照…');
    } catch (err) {
      set({ profileActive: false });
      get().pushToast('error', `恢复失败：${String(err)}`);
    }
  },
  profileDelete: async (userId) => {
    try {
      await api.profiles.delete(userId, get().profileApp);
      await get().refreshProfiles();
      get().pushToast('info', '快照已删除');
    } catch (err) {
      get().pushToast('error', `删除失败：${String(err)}`);
    }
  },
  oauthLogin: async (callbackUrl, accountName, groupId) => {
    try {
      const result = await api.oauth.login(callbackUrl, accountName, groupId);
      await get().refreshAccounts();
      get().pushToast('success', `OAuth 登录成功：账号「${result.name}」已添加`);
    } catch (err) {
      get().pushToast('error', `OAuth 登录失败：${String(err)}`);
      throw err;
    }
  },
  oauthLoginBitBrowser: async (windowId, accountName, groupId) => {
    try {
      const result = await api.oauth.loginBitBrowser(windowId, accountName, groupId);
      await get().refreshAccounts();
      const hours = result.token_exp_timestamp
        ? Math.max(0, (result.token_exp_timestamp * 1000 - Date.now()) / 3_600_000)
        : null;
      const suffix = hours != null ? `，JWT 约 ${hours.toFixed(1)} 小时后到期` : '';
      get().pushToast('success', `BitBrowser 原生 OAuth 接管成功：账号「${result.name}」已加入${suffix}`);
    } catch (err) {
      get().pushToast('error', `BitBrowser 原生 OAuth 登录失败：${String(err)}`);
      throw err;
    }
  },

  pushToast: (kind, msg) => {
    const configuredMode = get().settings?.notify;
    // 旧配置可能把 notify 保存为空字符串。空值或未知值都回退为应用内提示，
    // 避免操作已经执行但界面完全没有反馈。
    const mode = configuredMode === 'none'
      || configuredMode === 'toast'
      || configuredMode === 'system'
      || configuredMode === 'both'
      ? configuredMode
      : 'toast';
    if (mode === 'none') {
      console.debug('[notify] 已跳过（mode=none）:', kind, msg);
      return;
    }

    if (mode === 'toast' || mode === 'both') {
      const id = ++toastSeq;
      set((s) => ({ toasts: [...s.toasts, { id, kind, msg }] }));
      setTimeout(() => get().dismissToast(id), 4200);
    }

    if (mode === 'system' || mode === 'both') {
      // sendNotification v2 返回 void（fire-and-forget），用 try-catch 防御同步异常
      try {
        sendNotification({ title: APP_NAME, body: msg });
        console.debug('[notify] 系统通知已发送:', msg);
      } catch (e) {
        console.warn('[notify] sendNotification 异常:', e);
      }
    }
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));

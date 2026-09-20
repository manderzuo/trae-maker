/**
 * 全局 API 管理 · API Keys 管理（unified-api-gateway-design §5.2/§5.3）
 * 自 ApiService.tsx 原样搬移（props 化），行为不变：多 Key 签发 / 每日配额 / 鉴权开关 /
 * 子 Key 调度配置（F-35）。数据源：api_keys_list / api_keys_save（改动立即生效）。
 * 子弹框（子 Key 配置/删除确认）沿用 Modal 组件叠加；onSubModalChange 供主弹窗
 * 在子弹框打开期间屏蔽 ESC 双关（主弹窗 onClose 先于子弹框触发）。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { BarChart3, Copy, Gauge, KeyRound, Plus, Power, RefreshCw, Trash2 } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { maskApiKey, fmtTokens } from '../../lib/format';
import type {
  ApiKeyEntry,
  ApiKeyRuntimeUsage,
  GatewayLimitDefaults,
  KeyCapabilities,
  KeyCapability,
  KeyLimits,
  PoolStatus,
  UsageDayView,
} from '../../types';

export const DEFAULT_KEY_CAPABILITIES: KeyCapabilities = ['chat', 'video', 'assets'];

const DEFAULT_GATEWAY_LIMITS: GatewayLimitDefaults = {
  max_inflight: 32,
  max_video_jobs: 32,
  asset_uploads_per_minute: 30,
  asset_bytes_per_hour: 256 * 1024 * 1024,
  video_submissions_per_minute: 3,
};

type NullableKeyLimitField =
  | 'max_inflight'
  | 'max_video_jobs'
  | 'asset_uploads_per_minute'
  | 'asset_bytes_per_hour'
  | 'video_submissions_per_minute';

const NULLABLE_KEY_LIMIT_FIELDS: Array<{
  field: NullableKeyLimitField;
  label: string;
  unit: string;
  max: number;
}> = [
  { field: 'max_inflight', label: '文字请求并发', unit: '个', max: 256 },
  { field: 'max_video_jobs', label: '视频任务并发', unit: '个', max: 256 },
  { field: 'video_submissions_per_minute', label: '视频提交频率', unit: '次/分钟', max: 1_000 },
  { field: 'asset_uploads_per_minute', label: '素材上传频率', unit: '次/分钟', max: 10_000 },
  { field: 'asset_bytes_per_hour', label: '素材容量', unit: '字节/小时', max: 10 * 1024 * 1024 * 1024 },
];

const DAILY_KEY_LIMIT_FIELDS = [
  { field: 'daily_requests' as const, label: '每日请求额度', max: 1_000_000 },
  { field: 'daily_tokens' as const, label: '每日 Token 额度', max: 10_000_000_000 },
];

function normalizeOptionalLimit(value: number | null | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 1 ? Math.floor(value) : null;
}

function normalizeDailyLimit(value: number | null | undefined): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

/** 旧 Key 缺失 limits 时显示为跟随全局，daily_* 缺失时保持不限。 */
export function normalizeKeyLimits(limits?: Partial<KeyLimits> | null): KeyLimits {
  return {
    max_inflight: normalizeOptionalLimit(limits?.max_inflight),
    max_video_jobs: normalizeOptionalLimit(limits?.max_video_jobs),
    asset_uploads_per_minute: normalizeOptionalLimit(limits?.asset_uploads_per_minute),
    asset_bytes_per_hour: normalizeOptionalLimit(limits?.asset_bytes_per_hour),
    video_submissions_per_minute: normalizeOptionalLimit(limits?.video_submissions_per_minute),
    daily_requests: normalizeDailyLimit(limits?.daily_requests),
    daily_tokens: normalizeDailyLimit(limits?.daily_tokens),
  };
}

/** undefined/null 代表旧 Key，兼容地补齐三项能力；显式空数组仍表示全部禁用。 */
export function normalizeKeyCapabilities(capabilities?: KeyCapabilities | null): KeyCapabilities {
  if (capabilities == null) return [...DEFAULT_KEY_CAPABILITIES];
  return DEFAULT_KEY_CAPABILITIES.filter((capability) => capabilities.includes(capability));
}

export function validateKeyLimits(limits: KeyLimits): string | null {
  for (const { field, label, max } of DAILY_KEY_LIMIT_FIELDS) {
    const value = limits[field];
    if (!Number.isInteger(value) || value < 0 || value > max) {
      return `${label}需为 0-${max.toLocaleString('zh-CN')} 的整数`;
    }
  }
  for (const { field, label, max } of NULLABLE_KEY_LIMIT_FIELDS) {
    const value = limits[field];
    if (value != null && (!Number.isInteger(value) || value < 1 || value > max)) {
      return `${label}需为 1-${max.toLocaleString('zh-CN')} 的整数，或选择跟随全局`;
    }
  }
  return null;
}

export function effectiveKeyLimitSummary(
  limits: KeyLimits | null | undefined,
  global: GatewayLimitDefaults,
): GatewayLimitDefaults {
  const normalized = normalizeKeyLimits(limits);
  return {
    max_inflight: Math.min(normalized.max_inflight ?? global.max_inflight, global.max_inflight),
    max_video_jobs: Math.min(normalized.max_video_jobs ?? global.max_video_jobs, global.max_video_jobs),
    asset_uploads_per_minute: Math.min(
      normalized.asset_uploads_per_minute ?? global.asset_uploads_per_minute,
      global.asset_uploads_per_minute,
    ),
    asset_bytes_per_hour: Math.min(
      normalized.asset_bytes_per_hour ?? global.asset_bytes_per_hour,
      global.asset_bytes_per_hour,
    ),
    video_submissions_per_minute: Math.min(
      normalized.video_submissions_per_minute ?? global.video_submissions_per_minute,
      global.video_submissions_per_minute,
    ),
  };
}

export function buildKeyPolicyUpdate(
  key: ApiKeyEntry,
  limits: KeyLimits,
  capabilities: KeyCapabilities,
): ApiKeyEntry {
  return {
    ...key,
    // `daily_limit` is the persisted request-quota field. Keep the policy
    // mirror in sync so both the old list editor and the new modal enforce
    // the same value.
    daily_limit: normalizeDailyLimit(limits.daily_requests),
    limits: normalizeKeyLimits(limits),
    capabilities: normalizeKeyCapabilities(capabilities),
  };
}

function formatBytes(value: number): string {
  if (value >= 1024 * 1024 * 1024) return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
  if (value >= 1024 * 1024) return `${Math.round(value / (1024 * 1024))} MiB`;
  if (value >= 1024) return `${Math.round(value / 1024)} KiB`;
  return `${value} B`;
}

function normalizeGatewayLimits(limits?: Partial<GatewayLimitDefaults> | null): GatewayLimitDefaults {
  return {
    max_inflight: limits?.max_inflight ?? DEFAULT_GATEWAY_LIMITS.max_inflight,
    max_video_jobs: limits?.max_video_jobs ?? DEFAULT_GATEWAY_LIMITS.max_video_jobs,
    asset_uploads_per_minute:
      limits?.asset_uploads_per_minute ?? DEFAULT_GATEWAY_LIMITS.asset_uploads_per_minute,
    asset_bytes_per_hour: limits?.asset_bytes_per_hour ?? DEFAULT_GATEWAY_LIMITS.asset_bytes_per_hour,
    video_submissions_per_minute:
      limits?.video_submissions_per_minute ?? DEFAULT_GATEWAY_LIMITS.video_submissions_per_minute,
  };
}

export default function ApiKeysManager({
  onSubModalChange,
}: {
  /** 子弹框（子 Key 配置/删除确认）开关状态上报（主弹窗据此屏蔽 ESC 双关） */
  onSubModalChange?: (open: boolean) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  const [apiKeys, setApiKeys] = useState<ApiKeyEntry[]>([]);
  const [authDisabled, setAuthDisabled] = useState(false);
  const [keysSaving, setKeysSaving] = useState(false);
  const [newKeyName, setNewKeyName] = useState('');
  const [newKeyLimit, setNewKeyLimit] = useState(0);
  const [newKeyValue, setNewKeyValue] = useState('');
  // F-35 子 Key 配置弹框 + 删除确认（禁 window.confirm，红线）
  const [editKey, setEditKey] = useState<ApiKeyEntry | null>(null);
  const [policyKey, setPolicyKey] = useState<ApiKeyEntry | null>(null);
  const [policyLimits, setPolicyLimits] = useState<KeyLimits>(normalizeKeyLimits());
  const [policyCapabilities, setPolicyCapabilities] = useState<KeyCapabilities>([
    ...DEFAULT_KEY_CAPABILITIES,
  ]);
  const [policyError, setPolicyError] = useState('');
  const [editAllowed, setEditAllowed] = useState<Set<string>>(new Set());
  const [editMode, setEditMode] = useState('expire_first');
  const [editDedicated, setEditDedicated] = useState('');
  const [deleteForKey, setDeleteForKey] = useState<ApiKeyEntry | null>(null);
  // 子 Key 配置候选（服务运行中的上游账号；打开弹框时刷新）
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  // 今日按 Key 的 token 用量（「今日已用」列展示）
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  const [globalLimits, setGlobalLimits] = useState<GatewayLimitDefaults>(DEFAULT_GATEWAY_LIMITS);
  const [runtimeUsage, setRuntimeUsage] = useState<Record<string, ApiKeyRuntimeUsage>>({});
  const [bridgeStatus, setBridgeStatus] = useState<import('../../types').BridgeKeyStatusView | null>(null);
  const [issuedBridgeKey, setIssuedBridgeKey] = useState<import('../../types').IssuedBridgeKeyView | null>(null);

  // ---- 多 API Key 管理（原 ApiService.tsx 逻辑原样搬移） ----
  const loadKeys = useCallback(async () => {
    try {
      const view = await api.apiServer.keysList();
      setApiKeys(view.keys);
      setAuthDisabled(view.auth_disabled);
    } catch {
      /* 保留空列表 */
    }
  }, []);

  const loadRuntimeUsage = useCallback(async () => {
    try {
      const entries = await api.apiServer.keysUsage();
      setRuntimeUsage(Object.fromEntries(entries.map((entry) => [entry.key_id, entry])));
    } catch {
      /* 服务未运行时保留 0 占用 */
    }
  }, []);

  const loadBridgeStatus = useCallback(async () => {
    try {
      setBridgeStatus(await api.apiServer.bridgeKeyStatus());
    } catch {
      /* 旧版本或服务未初始化时保持默认提示 */
    }
  }, []);

  const issueBridgeKey = async () => {
    try {
      const issued = await api.apiServer.bridgeKeyIssue('星链维度分流系统桥接 Key');
      setIssuedBridgeKey(issued);
      await loadBridgeStatus();
      toast('success', '桥接 Key 已生成；明文只显示这一次，请立即复制到 Core');
    } catch (e) {
      toast('error', `生成桥接 Key 失败：${String(e).slice(0, 120)}`);
    }
  };

  const revokeBridgeKey = async () => {
    try {
      await api.apiServer.bridgeKeyRevoke();
      setIssuedBridgeKey(null);
      await loadBridgeStatus();
      toast('success', '桥接 Key 已撤销');
    } catch (e) {
      toast('error', `撤销桥接 Key 失败：${String(e).slice(0, 120)}`);
    }
  };

  /** 生成 sk- 前缀随机 Key（前端 crypto 随机源） */
  const generateKeyValue = useCallback(() => {
    const buf = new Uint8Array(24);
    crypto.getRandomValues(buf);
    // F-35：子 Key 统一 ck_ 前缀（旧 sk- Key 仍兼容鉴权）
    setNewKeyValue('ck_' + [...buf].map((b) => b.toString(16).padStart(2, '0')).join('').slice(0, 32));
  }, []);

  const saveKeys = async (next: ApiKeyEntry[], msg: string, nextAuthDisabled?: boolean) => {
    setKeysSaving(true);
    try {
      await api.apiServer.keysSave(next, nextAuthDisabled);
      try {
        const view = await api.apiServer.keysList();
        setApiKeys(view.keys);
        setAuthDisabled(view.auth_disabled);
      } catch {
        setApiKeys(next);
        if (nextAuthDisabled !== undefined) setAuthDisabled(nextAuthDisabled);
      }
      toast('success', msg);
    } catch (e) {
      toast('error', `保存 Key 失败：${String(e).slice(0, 120)}`);
    } finally {
      setKeysSaving(false);
    }
  };

  const toggleAuthDisabled = () => {
    const next = !authDisabled;
    void saveKeys(
      apiKeys,
      next ? '已关闭鉴权：无启用 Key 时任何本机程序均可调用（不推荐）' : '已开启鉴权：未配置启用 Key 时请求将被拒绝',
      next,
    );
  };

  const addKey = () => {
    const name = newKeyName.trim();
    if (!name) {
      toast('error', '请填写 Key 名称');
      return;
    }
    if (!newKeyValue.startsWith('ck_') && !newKeyValue.startsWith('sk-')) {
      toast('error', 'Key 值无效（需 ck_ 或 sk- 前缀），请重新生成');
      return;
    }
    if (apiKeys.some((k) => k.key === newKeyValue)) {
      toast('error', 'Key 值与现有条目重复');
      return;
    }
    const entry: ApiKeyEntry = {
      id: crypto.randomUUID(),
      name,
      key: newKeyValue,
      enabled: true,
      daily_limit: Math.max(0, Math.floor(newKeyLimit) || 0),
      created_at: Math.floor(Date.now() / 1000),
      used_date: '',
      used_today: 0,
      allowed_accounts: [],
      schedule_mode: 'expire_first',
      dedicated_account: '',
      daily_stats: [],
    };
    void saveKeys([...apiKeys, entry], `Key「${name}」已添加`);
    setNewKeyName('');
    setNewKeyLimit(0);
    generateKeyValue();
  };

  const toggleKey = (id: string) => {
    const next = apiKeys.map((k) => (k.id === id ? { ...k, enabled: !k.enabled } : k));
    void saveKeys(next, 'Key 状态已更新');
  };

  const deleteKey = (k: ApiKeyEntry) => {
    setDeleteForKey(k);
  };

  const confirmDeleteKey = async () => {
    if (!deleteForKey) return;
    const k = deleteForKey;
    setDeleteForKey(null);
    await saveKeys(apiKeys.filter((x) => x.id !== k.id), `Key「${k.name}」已删除`);
  };

  // 子 Key 配置弹框（F-35）：限定上游 + 专一/临期优先 + 按日统计展示
  const openKeyEdit = (k: ApiKeyEntry) => {
    setEditKey(k);
    setEditAllowed(new Set(k.allowed_accounts));
    setEditMode(k.schedule_mode || 'expire_first');
    setEditDedicated(k.dedicated_account || '');
    // 服务可能刚启动，候选列表即时刷新
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
  };

  const openPolicyEdit = (k: ApiKeyEntry) => {
    setPolicyKey(k);
    setPolicyLimits(normalizeKeyLimits(k.limits));
    setPolicyCapabilities(normalizeKeyCapabilities(k.capabilities));
    setPolicyError('');
  };

  const closePolicyEdit = () => {
    setPolicyKey(null);
    setPolicyError('');
  };

  const confirmKeyEdit = () => {
    if (!editKey) return;
    const next = apiKeys.map((k) =>
      k.id === editKey.id
        ? {
            ...k,
            allowed_accounts: [...editAllowed],
            schedule_mode: editMode,
            dedicated_account: editMode === 'dedicated' ? editDedicated : '',
          }
        : k,
    );
    void saveKeys(next, `Key「${editKey.name}」调度配置已更新`);
    setEditKey(null);
  };

  const confirmPolicyEdit = () => {
    if (!policyKey) return;
    const error = validateKeyLimits(policyLimits);
    if (error) {
      setPolicyError(error);
      toast('error', error);
      return;
    }
    const next = apiKeys.map((k) =>
      k.id === policyKey.id ? buildKeyPolicyUpdate(k, policyLimits, policyCapabilities) : k,
    );
    void saveKeys(next, `Key「${policyKey.name}」额度与限流已更新`);
    closePolicyEdit();
  };

  const setNullablePolicyLimit = (field: NullableKeyLimitField, mode: 'global' | 'custom') => {
    setPolicyError('');
    setPolicyLimits((current) => ({
      ...current,
      [field]: mode === 'global' ? null : current[field] ?? globalLimits[field],
    }));
  };

  const setPolicyCapability = (capability: KeyCapability) => {
    setPolicyError('');
    setPolicyCapabilities((current) =>
      current.includes(capability)
        ? current.filter((value) => value !== capability)
        : [...current, capability],
    );
  };

  const updateKeyLimit = (id: string, limit: number) => {
    const v = Math.max(0, Math.floor(limit) || 0);
    const cur = apiKeys.find((k) => k.id === id);
    if (!cur || cur.daily_limit === v) return;
    void saveKeys(
      apiKeys.map((k) =>
        k.id === id
          ? { ...k, daily_limit: v, limits: { ...normalizeKeyLimits(k.limits), daily_requests: v } }
          : k,
      ),
      '限额已更新',
    );
  };

  const copyKeyValue = async (k: ApiKeyEntry) => {
    try {
      await navigator.clipboard.writeText(k.key);
      toast('success', 'Key 已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    }
  };

  useEffect(() => {
    void loadKeys();
    void loadRuntimeUsage();
    void loadBridgeStatus();
    const usageTimer = window.setInterval(() => void loadRuntimeUsage(), 3000);
    generateKeyValue();
    // 上游账号候选（服务未运行时为空列表）
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    // 今日 token 用量（读落盘数据，仅取当日条目）
    api.apiServer
      .usageStats(14)
      .then(setUsage)
      .catch(() => {
        /* 保留空列表 */
      });
    api.apiServer
      .gatewaySettingsGet()
      .then((settings) => setGlobalLimits(normalizeGatewayLimits(settings.limit_defaults)))
      .catch(() => {
        /* 未读取到全局设置时使用后端默认值 */
      });
    return () => window.clearInterval(usageTimer);
  }, [loadKeys, loadRuntimeUsage, loadBridgeStatus, generateKeyValue]);

  // 子弹框开关状态上报（供主弹窗屏蔽 ESC 双关）
  useEffect(() => {
    onSubModalChange?.(editKey != null || policyKey != null || deleteForKey != null);
  }, [editKey, policyKey, deleteForKey, onSubModalChange]);

  // 今日按 Key 的 token 用量（Keys 表「今日已用」并列展示；日期口径与后端一致 = 本地时区 YYYY-MM-DD）
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const todayKeyTokens = useMemo(() => {
    const m = new Map<string, { prompt: number; completion: number }>();
    usage.find((d) => d.date === todayKey)?.key_tokens.forEach((t) =>
      m.set(t.name, { prompt: t.prompt_tokens, completion: t.completion_tokens }),
    );
    return m;
  }, [usage, todayKey]);

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <KeyRound size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">API Keys 管理</h3>
        <span className="hidden text-xs text-slate-400 sm:inline">
          多个 Key 独立签发并设置每日配额；增删/启停立即生效
        </span>
      </div>

      {/* 独立 Core 的唯一桥接凭据；普通用户 Key 由「星链维度分流系统」创建。 */}
      <div className="mb-3 rounded-lg border border-brand-200 bg-brand-50 p-3 text-xs dark:border-brand-900/50 dark:bg-brand-950/20">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <p className="font-medium text-slate-800 dark:text-zinc-100">星链维度分流系统桥接 Key</p>
            <p className="mt-1 text-slate-500 dark:text-zinc-400">
              AI Work 只负责执行和上游能力；普通用户、积分与额度由独立 Core 管理。
              {bridgeStatus?.bridge_only ? ' 当前已启用仅桥接模式。' : ' 生成后将自动切换为仅桥接模式。'}
            </p>
          </div>
          <div className="flex items-center gap-2">
            {bridgeStatus?.active && !bridgeStatus.active.revoked ? (
              <button className="btn-outline flex items-center gap-1 !px-3 text-xs" onClick={() => void revokeBridgeKey()}>
                <Power size={13} /> 撤销并停止桥接
              </button>
            ) : (
              <button className="btn-primary flex items-center gap-1 !px-3 text-xs" onClick={() => void issueBridgeKey()}>
                <KeyRound size={13} /> 生成桥接 Key
              </button>
            )}
          </div>
        </div>
        {bridgeStatus?.active && (
          <p className="mt-2 font-mono text-[11px] text-slate-500 dark:text-zinc-400">
            当前状态：{bridgeStatus.active.enabled ? '启用' : '已撤销'} · {bridgeStatus.active.key_prefix}… · ID {bridgeStatus.active.id}
          </p>
        )}
        {issuedBridgeKey && (
          <div className="mt-3 rounded border border-amber-300 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950/30">
            <p className="font-medium text-amber-800 dark:text-amber-200">请立即复制：此明文只显示本次</p>
            <div className="mt-1 flex items-center gap-2">
              <code className="min-w-0 flex-1 break-all rounded bg-white px-2 py-1 font-mono text-[11px] dark:bg-zinc-900">{issuedBridgeKey.plaintext}</code>
              <button className="btn-ghost shrink-0 !p-1" onClick={() => void navigator.clipboard.writeText(issuedBridgeKey.plaintext)} title="复制桥接 Key">
                <Copy size={13} />
              </button>
            </div>
          </div>
        )}
      </div>

      <div className="mb-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-800/50">
        <p className="font-medium text-slate-700 dark:text-zinc-200">旧 API Key 仅作为迁移信息保留</p>
        <p className="text-slate-400 dark:text-zinc-500">不再从 AI Work 新建普通用户 Key；请在独立 Core 的用户与 Key 管理页操作。</p>
      </div>

      {apiKeys.length === 0 ? (
        <p className="py-4 text-center text-sm text-slate-400">
          {authDisabled
            ? '暂无 Key — 鉴权已关闭，任何本机程序无需 Key 即可调用'
            : '暂无 Key — 请求将被拒绝；请添加并启用 Key，或关闭鉴权'}
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                <th className="pb-2 pr-4 font-medium">名称</th>
                <th className="pb-2 pr-4 font-medium">Key</th>
                <th className="pb-2 pr-4 font-medium">日限额(次)</th>
                <th className="pb-2 pr-4 font-medium">今日已用(次/tok)</th>
                <th className="pb-2 pr-4 font-medium">调度 / 有效限流</th>
                <th className="pb-2 pr-4 font-medium">状态</th>
                <th className="pb-2 font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {apiKeys.map((k) => {
                const exhausted = k.daily_limit > 0 && k.used_today >= k.daily_limit;
                const kt = todayKeyTokens.get(k.id);
                const ktTotal = kt ? kt.prompt + kt.completion : 0;
                const effective = effectiveKeyLimitSummary(k.limits, globalLimits);
                const live = runtimeUsage[k.id] ?? { inflight: 0, video_jobs: 0 };
                return (
                  <tr
                    key={k.id}
                    className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"
                  >
                    <td className="py-2 pr-4 font-medium text-slate-700 dark:text-zinc-200">
                      {k.name}
                    </td>
                    <td className="py-2 pr-4 font-mono text-xs text-slate-500 dark:text-zinc-400">
                      {maskApiKey(k.key)}
                    </td>
                    <td className="py-2 pr-4">
                      <input
                        type="number"
                        min={0}
                        className="input !w-24 !px-2 !py-1 text-xs"
                        defaultValue={k.daily_limit}
                        onBlur={(e) => {
                          const raw = e.target.value.trim();
                          if (!/^\d+$/.test(raw)) {
                            // 空/非法输入不落 0（不限），还原显示并提示
                            e.target.value = String(k.daily_limit);
                            toast('error', '日限额需为非负整数，已还原原值');
                            return;
                          }
                          updateKeyLimit(k.id, parseInt(raw, 10));
                        }}
                        title="0 表示不限；失焦自动保存"
                      />
                    </td>
                    <td
                      className={
                        'py-2 pr-4 tabular-nums ' +
                        (exhausted
                          ? 'font-semibold text-amber-600 dark:text-amber-400'
                          : 'text-slate-500 dark:text-zinc-400')
                      }
                    >
                      {k.used_today}
                      {k.daily_limit > 0 ? ` / ${k.daily_limit}` : ''} 次
                      {ktTotal > 0 && ` · ${fmtTokens(ktTotal)} tok`}
                    </td>
                    <td className="py-2 pr-4">
                      {(k.schedule_mode || 'expire_first') === 'dedicated' ? (
                        <Badge tone="violet">专一</Badge>
                      ) : (
                        <Badge tone="slate">临期优先</Badge>
                      )}
                      {k.allowed_accounts.length > 0 && (
                        <span className="ml-1 text-xs text-slate-400" title={k.allowed_accounts.join(', ')}>
                          限{k.allowed_accounts.length}账号
                        </span>
                      )}
                      <div className="mt-1 max-w-[21rem] text-[10px] leading-4 text-slate-400 dark:text-zinc-500">
                        并发 {live.inflight}/{effective.max_inflight} · 视频任务 {live.video_jobs}/{effective.max_video_jobs} · 视频提交{' '}
                        {effective.video_submissions_per_minute}/分 · 素材 {effective.asset_uploads_per_minute}/分 ·{' '}
                        {formatBytes(effective.asset_bytes_per_hour)}/时
                      </div>
                    </td>
                    <td className="py-2 pr-4">
                      {k.enabled ? <Badge tone="green">启用中</Badge> : <Badge tone="slate">已禁用</Badge>}
                    </td>
                    <td className="py-2">
                      <div className="flex items-center gap-1">
                        <button
                          className="btn-ghost !p-1.5"
                          title={k.enabled ? '禁用' : '启用'}
                          onClick={() => toggleKey(k.id)}
                          disabled={keysSaving}
                        >
                          <Power
                            size={14}
                            className={k.enabled ? 'text-emerald-500' : 'text-slate-400'}
                          />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="复制完整 Key"
                          onClick={() => void copyKeyValue(k)}
                        >
                          <Copy size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="调度配置（限定上游 / 专一 / 临期优先）"
                          onClick={() => openKeyEdit(k)}
                        >
                          <BarChart3 size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="额度与限流（每日额度 / 并发 / 视频 / 素材 / 能力）"
                          onClick={() => openPolicyEdit(k)}
                        >
                          <Gauge size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="删除"
                          onClick={() => deleteKey(k)}
                          disabled={keysSaving}
                        >
                          <Trash2 size={14} className="text-rose-500" />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* Key 额度与限流配置：nullable 限制显式选择跟随全局，daily=0 表示不限 */}
      <Modal
        open={policyKey != null}
        onClose={closePolicyEdit}
        title={`额度与限流 · ${policyKey?.name ?? ''}`}
        size="xl"
        bodyClass="max-h-[70vh] overflow-y-auto"
        footer={
          <>
            <button className="btn-outline" onClick={closePolicyEdit}>取消</button>
            <button className="btn-primary" onClick={confirmPolicyEdit} disabled={keysSaving}>保存</button>
          </>
        }
      >
        <div className="space-y-4">
          <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
            旧 Key 缺失新字段时默认跟随全局并允许 chat / video / assets。Key 级限制只能收紧全局值，空白限制请选择“跟随全局”。
          </div>

          <div>
            <div className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">每日额度</div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              {DAILY_KEY_LIMIT_FIELDS.map(({ field, label, max }) => {
                const current = policyKey
                  ? field === 'daily_requests'
                    ? policyKey.used_today
                    : (todayKeyTokens.get(policyKey.id)?.prompt ?? 0) + (todayKeyTokens.get(policyKey.id)?.completion ?? 0)
                  : 0;
                return (
                  <label key={field} className="block">
                    <span className="mb-1 block text-xs font-medium text-slate-600 dark:text-zinc-300">{label}</span>
                    <input
                      className="input"
                      type="number"
                      min={0}
                      max={max}
                      step={1}
                      value={policyLimits[field]}
                      onChange={(e) => {
                        setPolicyError('');
                        setPolicyLimits((value) => ({ ...value, [field]: Number(e.target.value) }));
                      }}
                    />
                    <span className="mt-1 block text-[11px] text-slate-400">
                      0 = 不限；今日已用 {field === 'daily_tokens' ? `${fmtTokens(current)} Token` : `${current} 次`}
                    </span>
                  </label>
                );
              })}
            </div>
          </div>

          <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
            <div className="mb-2 flex flex-wrap items-baseline justify-between gap-2">
              <div>
                <p className="text-xs font-medium text-slate-600 dark:text-zinc-300">限流覆盖</p>
                <p className="mt-1 text-[11px] text-slate-400">跟随全局的值会随接口配置变化；自定义值大于全局值时仍按全局上限生效。</p>
              </div>
              <span className="text-[11px] text-slate-400">
                全局：并发 {globalLimits.max_inflight} · 视频 {globalLimits.max_video_jobs} · 视频提交{' '}
                {globalLimits.video_submissions_per_minute}/分
              </span>
            </div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              {NULLABLE_KEY_LIMIT_FIELDS.map(({ field, label, unit, max }) => {
                const value = policyLimits[field];
                const effective = effectiveKeyLimitSummary(policyLimits, globalLimits)[field];
                return (
                  <div key={field}>
                    <label className="mb-1 block text-xs font-medium text-slate-600 dark:text-zinc-300">{label}</label>
                    <div className="flex gap-2">
                      <select
                        className="input w-32 shrink-0"
                        value={value == null ? 'global' : 'custom'}
                        aria-label={`${label}来源`}
                        onChange={(e) => setNullablePolicyLimit(field, e.target.value as 'global' | 'custom')}
                      >
                        <option value="global">跟随全局</option>
                        <option value="custom">自定义</option>
                      </select>
                      <input
                        className="input min-w-0 flex-1"
                        type="number"
                        min={1}
                        max={max}
                        step={1}
                        value={value ?? ''}
                        disabled={value == null}
                        onChange={(e) => {
                          setPolicyError('');
                          setPolicyLimits((current) => ({
                            ...current,
                            [field]: Number(e.target.value),
                          }));
                        }}
                        placeholder={String(globalLimits[field])}
                      />
                    </div>
                    <span className="mt-1 block text-[11px] text-slate-400">
                      有效 {field === 'asset_bytes_per_hour' ? formatBytes(effective) : `${effective} ${unit}`}
                      {value == null ? '（全局）' : ''}
                    </span>
                  </div>
                );
              })}
            </div>
          </div>

          <div>
            <div className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">能力开关</div>
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
              {(
                [
                  { key: 'chat' as const, label: 'chat', desc: '文字与兼容消息接口' },
                  { key: 'video' as const, label: 'video', desc: '视频提交、查询与下载' },
                  { key: 'assets' as const, label: 'assets', desc: '参考图/视频素材上传' },
                ] satisfies Array<{ key: KeyCapability; label: string; desc: string }>
              ).map((capability) => (
                <label
                  key={capability.key}
                  className="flex cursor-pointer items-start gap-2 rounded-lg border border-slate-200 p-2.5 text-xs dark:border-zinc-700"
                >
                  <input
                    type="checkbox"
                    checked={policyCapabilities.includes(capability.key)}
                    onChange={() => setPolicyCapability(capability.key)}
                  />
                  <span>
                    <span className="block font-medium text-slate-700 dark:text-zinc-200">{capability.label}</span>
                    <span className="mt-0.5 block text-slate-400">{capability.desc}</span>
                  </span>
                </label>
              ))}
            </div>
          </div>

          {policyError && (
            <p className="rounded-lg bg-rose-50 px-3 py-2 text-xs text-rose-600 dark:bg-rose-500/10 dark:text-rose-300">
              {policyError}
            </p>
          )}
        </div>
      </Modal>

      {/* 子 Key 调度配置弹框（F-35：限定上游 + 专一/临期优先 + 按日统计） */}
      <Modal
        open={editKey != null}
        onClose={() => setEditKey(null)}
        title={`调度配置 · ${editKey?.name ?? ''}`}
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditKey(null)}>取消</button>
            <button className="btn-primary" onClick={confirmKeyEdit} disabled={keysSaving}>保存</button>
          </>
        }
      >
        <div className="space-y-4 text-sm">
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">调度模式</div>
            <div className="flex gap-2">
              {[
                { key: 'expire_first', label: '临期优先', desc: '按积分最早到期取上游' },
                { key: 'dedicated', label: '专一', desc: '固定绑定单一上游账号' },
              ].map((m) => (
                <button
                  key={m.key}
                  className={`flex-1 rounded-lg border p-2.5 text-left text-xs ${editMode === m.key ? 'border-indigo-400 bg-indigo-50 dark:bg-indigo-500/10' : 'border-slate-200 dark:border-zinc-700'}`}
                  onClick={() => setEditMode(m.key)}
                >
                  <div className="font-medium">{m.label}</div>
                  <div className="mt-0.5 text-slate-400">{m.desc}</div>
                </button>
              ))}
            </div>
          </div>
          {editMode === 'dedicated' && (
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">专一账号</span>
              <select className="input w-full" value={editDedicated} onChange={(e) => setEditDedicated(e.target.value)}>
                <option value="">— 默认取限定上游首个 —</option>
                {poolStatus.map((p) => (
                  <option key={p.uid} value={p.uid}>
                    {p.name || p.uid}
                    {p.credits != null ? `（${p.credits.toFixed(1)} 积分）` : ''}
                  </option>
                ))}
              </select>
              {poolStatus.length === 0 && (
                <span className="mt-1 block text-xs text-amber-500">服务未运行，暂无上游账号候选；可保存后稍后调整。</span>
              )}
            </label>
          )}
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">
              限定上游<span className="ml-1 font-normal text-slate-400">（不勾选 = 使用全部上游账号）</span>
            </div>
            <div className="max-h-40 space-y-1 overflow-y-auto rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
              {poolStatus.length === 0 ? (
                <div className="py-2 text-center text-xs text-slate-400">服务未运行，暂无上游账号候选</div>
              ) : (
                poolStatus.map((p) => (
                  <label key={p.uid} className="flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      checked={editAllowed.has(p.uid)}
                      onChange={() => {
                        const next = new Set(editAllowed);
                        if (next.has(p.uid)) next.delete(p.uid);
                        else next.add(p.uid);
                        setEditAllowed(next);
                      }}
                    />
                    <span className="truncate">{p.name || p.uid}</span>
                    {p.credits != null && <span className="ml-auto tabular-nums text-slate-400">{p.credits.toFixed(1)}</span>}
                  </label>
                ))
              )}
            </div>
          </div>
          {editKey && (editKey.daily_stats?.length ?? 0) > 0 && (
            <div>
              <div className="mb-1.5 text-xs font-medium text-slate-500">近 7 日请求统计</div>
              <div className="flex items-end gap-1.5">
                {editKey.daily_stats.slice(-7).map((d) => {
                  const max = Math.max(...editKey.daily_stats.slice(-7).map((x) => x.requests), 1);
                  return (
                    <div key={d.date} className="flex flex-1 flex-col items-center gap-1" title={`${d.date}：${d.requests} 次`}>
                      <span className="text-[10px] tabular-nums text-slate-400">{d.requests}</span>
                      <div
                        className="w-full rounded-t bg-indigo-400"
                        style={{ height: `${Math.max(4, (d.requests / max) * 40)}px` }}
                      />
                      <span className="text-[10px] text-slate-400">{d.date.slice(5)}</span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </Modal>

      {/* Key 删除确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteForKey != null}
        onClose={() => setDeleteForKey(null)}
        title="删除 API Key"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteForKey(null)}>取消</button>
            <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={() => void confirmDeleteKey()}>确认删除</button>
          </>
        }
      >
        <div className="text-sm">
          确认删除 Key「{deleteForKey?.name}」？
          <div className="mt-1 text-xs text-slate-400">使用该 Key 的客户端将立即无法访问（401）。</div>
        </div>
      </Modal>
    </div>
  );
}

/**
 * 全局 API 管理 · 接口配置（unified-api-gateway-design §5.2/§5.3）
 * 读写 api_gateway_settings.json（gateway_settings_get/set，Phase 1 §8.1）；
 * 端口改动下次启动 API 服务后生效；含使用方式与配置示例。
 */
import { useEffect, useState } from 'react';
import { Copy, Download, Globe, Save, Link, Radio, Square, Trash2 } from 'lucide-react';
import { api } from '../../lib/tauri';
import { withMinDelay } from '../../lib/delay';
import { useAppStore } from '../../store';
import type { ConversationSettings, ConversationSummary, FrpConfig, FrpStatus, GatewaySettings, TunnelConfig, TunnelStatus, UnifiedModel } from '../../types';

export default function InterfaceConfig() {
  const toast = useAppStore((s) => s.pushToast);
  const [gw, setGw] = useState<GatewaySettings | null>(null);
  const [port, setPort] = useState(7864);
  const [listenHost, setListenHost] = useState('127.0.0.1');
  const [corsOrigins, setCorsOrigins] = useState('');
  const [assetPublicBaseUrl, setAssetPublicBaseUrl] = useState('');
  const [model, setModel] = useState('glm-5.3');
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [saving, setSaving] = useState(false);
  const [copying, setCopying] = useState(false);
  const [tunnel, setTunnel] = useState<TunnelConfig>({
    host: '', username: '', key_path: '',
    remote_port: 7864, ssh_port: 22, local_host: '127.0.0.1', local_port: 7864, remote_bind: '127.0.0.1',
    auto_reconnect: false, host_key_fingerprint: '',
  });
  const [tunnelStatus, setTunnelStatus] = useState<TunnelStatus | null>(null);
  const [tunnelBusy, setTunnelBusy] = useState(false);
  const [frp, setFrp] = useState<FrpConfig>({
    binary_path: '', server_addr: '', server_port: 7000, proxy_name: 'aiwork-gateway',
    local_host: '127.0.0.1', local_port: 7864, remote_port: 17864, auth_token: '',
    tls_enable: true, auto_reconnect: false,
  });
  const [frpStatus, setFrpStatus] = useState<FrpStatus | null>(null);
  const [frpBusy, setFrpBusy] = useState(false);
  const [conversationCount, setConversationCount] = useState<number | null>(null);
  const [conversationItems, setConversationItems] = useState<ConversationSummary[]>([]);
  const [archiveBusy, setArchiveBusy] = useState(false);
  const [conversationSettings, setConversationSettings] = useState<ConversationSettings>({ persist_body: true, retention_days: 0 });
  const [conversationSaving, setConversationSaving] = useState(false);

  useEffect(() => {
    api.apiServer
      .gatewaySettingsGet()
      .then((s) => {
        setGw(s);
        setPort(s.port);
        setListenHost(s.listen_host || '127.0.0.1');
        setCorsOrigins(s.cors_origins || '');
        setAssetPublicBaseUrl(s.asset_public_base_url || '');
        setModel(s.default_model);
      })
      .catch(() => {
        /* 保留默认值 */
      });
    api.apiServer
      .unifiedModels()
      .then(setModels)
      .catch(() => {
        /* 保留空列表 */
      });
    api.apiServer.tunnelGet().then(setTunnel).catch(() => { /* 使用默认值 */ });
    api.apiServer.tunnelStatus().then(setTunnelStatus).catch(() => { /* 状态稍后重试 */ });
    api.apiServer.frpGet().then(setFrp).catch(() => { /* 使用默认值 */ });
    api.apiServer.frpStatus().then(setFrpStatus).catch(() => { /* 状态稍后重试 */ });
    api.apiServer.conversationsList().then((items) => { setConversationItems(items); setConversationCount(items.length); }).catch(() => setConversationCount(null));
    api.apiServer.conversationsSettingsGet().then(setConversationSettings).catch(() => { /* 使用兼容默认值 */ });
  }, []);

  const save = async () => {
    const p = Math.floor(port);
    if (!Number.isFinite(p) || p < 1 || p > 65535) {
      toast('error', '端口需为 1-65535 的整数');
      return;
    }
    setSaving(true);
    try {
      // 后端会规范化（空模型名回退默认值），前端展示以返回值为准（§5.3）
      const next = await withMinDelay(
        api.apiServer.gatewaySettingsSet({
          port: p,
          default_model: model.trim(),
          listen_host: listenHost.trim() || '127.0.0.1',
          cors_origins: corsOrigins.trim(),
          asset_public_base_url: assetPublicBaseUrl.trim(),
          updated_at: gw?.updated_at ?? 0,
        }),
      );
      setGw(next);
      setPort(next.port);
      setListenHost(next.listen_host || '127.0.0.1');
      setCorsOrigins(next.cors_origins || '');
      setAssetPublicBaseUrl(next.asset_public_base_url || '');
      setModel(next.default_model);
      toast('success', '网关设置已保存；端口改动将在下次启动 API 服务后生效');
    } catch (e) {
      toast('error', `保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  const copyConfigExample = async () => {
    const p = gw?.port ?? 7864;
    const host = gw?.listen_host ?? listenHost ?? '127.0.0.1';
    const m = gw?.default_model ?? 'glm-5.3';
    const example = `# 客户端配置示例（OpenAI 兼容格式）
接口地址: http://${host}:${p}/v1
API Key:  <在「API Keys 管理」中创建并复制>
模型 ID:  ${m}（统一目录内任一模型均可，请求按模型 ID 匹配资源池）

# Anthropic 兼容端点（Claude Code 等工具直连）
POST http://${host}:${p}/v1/messages
鉴权头: x-api-key: your-api-key 或 Authorization: Bearer

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST http://${host}:${p}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${m}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'

# Seedance 视频生成（Trae Work CN Work 积分；异步任务）
curl -X POST http://${host}:${p}/v1/videos/generations \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -H "Idempotency-Key: demo-video-001" \\
  -d '{
    "model": "seedance",
    "prompt": "一只猫在窗边看雨，电影感镜头",
    "duration": 4,
    "resolution": "720p",
    "ratio": "16:9"
  }'
# 返回 task.id 后轮询：GET http://${host}:${p}/v1/videos/<task_id>
# 下载网关缓存：GET http://${host}:${p}/v1/videos/<task_id>/content

# 参考图（调用方先读文件，再 Base64 上传；网关按 API Key 隔离）
POST http://${host}:${p}/v1/assets
{
  "filename": "reference.png",
  "mime_type": "image/png",
  "data_base64": "<图片 Base64 或 data:image/png;base64,...>"
}
# 返回 asset.id 后，可在视频请求中使用 image_asset_ids。
# 仅当网关配置了 Trae 可访问的 AIWORK_ASSET_PUBLIC_BASE_URL 时，上游才会回取素材。
# 本机/局域网地址（127.0.0.1、192.168.x.x）通常无法被 Trae 云端回取；公网部署请使用 HTTPS 域名。
`;
    setCopying(true);
    try {
      await withMinDelay(navigator.clipboard.writeText(example));
      toast('success', '配置示例已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    } finally {
      setCopying(false);
    }
  };

  const saveTunnel = async () => {
    setTunnelBusy(true);
    try {
      const next = await api.apiServer.tunnelSave(tunnel);
      setTunnel(next);
      toast('success', 'SSH 中转配置已保存（尚未建立连接）');
    } catch (e) {
      toast('error', `保存 SSH 配置失败：${String(e).slice(0, 120)}`);
    } finally {
      setTunnelBusy(false);
    }
  };

  const startTunnel = async () => {
    setTunnelBusy(true);
    try {
      const next = await api.apiServer.tunnelStart(tunnel);
      setTunnel(next.config);
      setTunnelStatus(next);
      toast('success', 'SSH 反向隧道已启动');
    } catch (e) {
      toast('error', `启动 SSH 中转失败：${String(e).slice(0, 160)}`);
    } finally {
      setTunnelBusy(false);
    }
  };

  const stopTunnel = async () => {
    setTunnelBusy(true);
    try {
      await api.apiServer.tunnelStop();
      setTunnelStatus(await api.apiServer.tunnelStatus());
      toast('info', 'SSH 反向隧道已停止');
    } catch (e) {
      toast('error', `停止 SSH 中转失败：${String(e).slice(0, 120)}`);
    } finally {
      setTunnelBusy(false);
    }
  };

  const saveFrp = async () => {
    setFrpBusy(true);
    try {
      const next = await api.apiServer.frpSave(frp);
      setFrp(next);
      toast('success', 'FRP 配置已保存（尚未建立连接）');
    } catch (e) {
      toast('error', `保存 FRP 配置失败：${String(e).slice(0, 160)}`);
    } finally {
      setFrpBusy(false);
    }
  };

  const startFrp = async () => {
    setFrpBusy(true);
    try {
      const next = await api.apiServer.frpStart(frp);
      setFrp(next.config);
      setFrpStatus(next);
      toast('success', 'frpc 已启动；请从其他网段检查 Nginx/health');
    } catch (e) {
      toast('error', `启动 FRP 失败：${String(e).slice(0, 180)}`);
    } finally {
      setFrpBusy(false);
    }
  };

  const stopFrp = async () => {
    setFrpBusy(true);
    try {
      await api.apiServer.frpStop();
      setFrpStatus(await api.apiServer.frpStatus());
      toast('info', 'frpc 已停止');
    } catch (e) {
      toast('error', `停止 FRP 失败：${String(e).slice(0, 120)}`);
    } finally {
      setFrpBusy(false);
    }
  };

  const exportConversations = async () => {
    setArchiveBusy(true);
    try {
      const result = await api.apiServer.conversationsExport();
      setConversationCount(result.conversations);
      setConversationItems(await api.apiServer.conversationsList());
      if (result.conversations === 0) {
        toast('info', '当前没有可导出的本地 API 会话；完成一次带 conversation_id 的请求后再试');
      } else {
        toast('success', `会话存档已导出：${result.conversations} 个会话 / ${result.messages} 条消息`);
      }
    } catch (e) {
      toast('error', `导出会话存档失败：${String(e).slice(0, 160)}`);
    } finally {
      setArchiveBusy(false);
    }
  };

  const saveConversationSettings = async () => {
    setConversationSaving(true);
    try {
      const next = await api.apiServer.conversationsSettingsSet({
        persist_body: conversationSettings.persist_body,
        retention_days: Math.max(0, Math.min(3650, Math.floor(Number(conversationSettings.retention_days) || 0))),
      });
      setConversationSettings(next);
      const list = await api.apiServer.conversationsList();
      setConversationItems(list);
      setConversationCount(list.length);
      toast('success', '本机会话存档设置已保存');
    } catch (e) {
      toast('error', `保存会话设置失败：${String(e).slice(0, 160)}`);
    } finally {
      setConversationSaving(false);
    }
  };

  const deleteConversation = async (id: string) => {
    if (!window.confirm(`确定删除本机会话「${id}」及其全部消息吗？此操作不可撤销。`)) return;
    try {
      const deleted = await api.apiServer.conversationsDelete(id);
      if (!deleted) {
        toast('info', '会话已不存在');
        return;
      }
      const list = await api.apiServer.conversationsList();
      setConversationItems(list);
      setConversationCount(list.length);
      toast('success', '会话及其本地消息已删除');
    } catch (e) {
      toast('error', `删除会话失败：${String(e).slice(0, 160)}`);
    }
  };

  // 默认模型不在目录中时（如目录尚未聚合），前置占位避免显示错位
  const modelOptions =
    models.some((m) => m.id === model) || !model
      ? models
      : [{ id: model, display: model, rate: null, efforts: [], context_length: null, max_tokens: null, supports_image: null, manual: false, sources: [] }, ...models];

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <Globe size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">接口配置</h3>
        <span className="text-xs text-slate-400">读写 api_gateway_settings.json</span>
      </div>

      <div className="space-y-4">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              监听地址
            </label>
            <input
              className="input"
              value={listenHost}
              onChange={(e) => setListenHost(e.target.value)}
              placeholder="127.0.0.1（局域网填 0.0.0.0）"
            />
            <p className="mt-1 text-xs text-slate-400">默认仅本机；0.0.0.0 会暴露到局域网，请务必启用 API Key。服务器部署可用 AIWORK_BIND 覆盖。</p>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              监听端口
            </label>
            <input
              type="number"
              className="input"
              value={port}
              onChange={(e) => setPort(parseInt(e.target.value) || 0)}
            />
            <p className="mt-1 text-xs text-slate-400">改动将在下次启动 API 服务后生效；服务器可用 AIWORK_PORT 覆盖</p>
          </div>
          <div className="sm:col-span-2">
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              浏览器 CORS 来源（可选）
            </label>
            <input
              className="input"
              value={corsOrigins}
              onChange={(e) => setCorsOrigins(e.target.value)}
              placeholder="例如 http://localhost:3000, https://你的域名"
            />
            <p className="mt-1 text-xs text-slate-400">仅允许列出的 Origin；留空时不开放浏览器跨域。局域网监听仍必须配置 API Key。</p>
          </div>
          <div className="sm:col-span-2">
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              参考素材公开基址（可选）
            </label>
            <input
              className="input"
              value={assetPublicBaseUrl}
              onChange={(e) => setAssetPublicBaseUrl(e.target.value)}
              placeholder="例如 https://api.example.com/v1"
            />
            <p className="mt-1 text-xs text-slate-400">
              用于图生视频：Trae 云端需要从这里临时读取参考图/视频。必须是 Trae 可访问的 http(s) 地址，不能填 127.0.0.1 或仅办公室内网地址；留空时仅保存本机素材，不会把它交给上游。云端部署可用 AIWORK_ASSET_PUBLIC_BASE_URL 覆盖。
            </p>
          </div>
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              默认模型
            </label>
            <select
              className="input"
              value={model}
              onChange={(e) => setModel(e.target.value)}
            >
              {modelOptions.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.display || m.id}
                  {m.rate != null ? `（${m.rate.toFixed(2)}x）` : ''}
                </option>
              ))}
            </select>
            <p className="mt-1 text-xs text-slate-400">
              统一目录（Trae / Buddy 聚合）；用于 CC Switch 注册与未指定 model 的请求。服务器可用 AIWORK_DEFAULT_MODEL 覆盖
            </p>
          </div>
        </div>

        <button
          className="btn-secondary flex items-center gap-2"
          onClick={() => void save()}
          disabled={saving}
        >
          <Save size={15} />
          {saving ? '保存中…' : '保存配置'}
        </button>

        <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <div className="mb-3 flex items-center gap-2">
            <Link size={15} className="text-brand-500" />
            <p className="text-sm font-semibold text-slate-700 dark:text-zinc-200">腾讯云 SSH 安全中转（可选）</p>
            <span className={`text-xs ${tunnelStatus?.running ? 'text-emerald-600' : 'text-slate-400'}`}>
              {tunnelStatus?.running ? '运行中' : '未连接'}
            </span>
          </div>
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <input className="input" value={tunnel.host} onChange={(e) => setTunnel({ ...tunnel, host: e.target.value })} placeholder="腾讯云公网 IP / 域名" />
            <input className="input" value={tunnel.username} onChange={(e) => setTunnel({ ...tunnel, username: e.target.value })} placeholder="SSH 用户名（如 ubuntu）" />
            <input className="input sm:col-span-2" value={tunnel.key_path} onChange={(e) => setTunnel({ ...tunnel, key_path: e.target.value })} placeholder="PEM 私钥完整路径" />
            <input className="input" type="number" value={tunnel.remote_port} onChange={(e) => setTunnel({ ...tunnel, remote_port: Number(e.target.value) || 0 })} placeholder="远端端口" />
            <input className="input" type="number" value={tunnel.ssh_port} onChange={(e) => setTunnel({ ...tunnel, ssh_port: Number(e.target.value) || 0 })} placeholder="SSH 端口（默认 22）" />
            <input className="input" type="number" value={tunnel.local_port} onChange={(e) => setTunnel({ ...tunnel, local_port: Number(e.target.value) || 0 })} placeholder="本机网关端口" />
            <input className="input sm:col-span-2" value={tunnel.host_key_fingerprint} onChange={(e) => setTunnel({ ...tunnel, host_key_fingerprint: e.target.value })} placeholder="可选主机指纹（SHA256:...，不填则依赖 known_hosts）" />
          </div>
          <label className="mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-zinc-400">
            <input type="checkbox" checked={tunnel.auto_reconnect} onChange={(e) => setTunnel({ ...tunnel, auto_reconnect: e.target.checked })} />
            断线自动重连（仅在 host key 已核对后启用）
          </label>
          <p className="mt-2 text-xs text-slate-400">仅保存连接参数；SSH 使用 StrictHostKeyChecking=yes，不会静默接受陌生主机；私钥不会被读取或上传。</p>
          <div className="mt-3 flex flex-wrap gap-2">
            <button className="btn-secondary" onClick={() => void saveTunnel()} disabled={tunnelBusy}>保存参数</button>
            <button className="btn-primary" onClick={() => void startTunnel()} disabled={tunnelBusy || tunnelStatus?.running === true}>启动中转</button>
            <button className="btn-ghost" onClick={() => void stopTunnel()} disabled={tunnelBusy || tunnelStatus?.running !== true}><Square size={13} />停止</button>
          </div>
          {tunnelStatus?.message && <p className="mt-2 text-xs text-slate-500 dark:text-zinc-400">{tunnelStatus.message}</p>}
        </div>

        <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <div className="mb-3 flex items-center gap-2">
            <Radio size={15} className="text-brand-500" />
            <p className="text-sm font-semibold text-slate-700 dark:text-zinc-200">FRP 局域网 / 公网接入</p>
            <span className={`text-xs ${frpStatus?.running ? 'text-emerald-600' : 'text-slate-400'}`}>
              {frpStatus?.running ? '运行中' : '未连接'}
            </span>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            frpc 运行在本机，主动连接局域网中转机或腾讯云 frps；Nginx 放在中转机统一提供 HTTP/HTTPS。此功能只转发 API，不构成完整二层局域网桥接。
          </p>
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <input className="input sm:col-span-2" value={frp.binary_path} onChange={(e) => setFrp({ ...frp, binary_path: e.target.value })} placeholder="frpc 可执行文件路径（留空则从 PATH 查找）" />
            <input className="input" value={frp.server_addr} onChange={(e) => setFrp({ ...frp, server_addr: e.target.value })} placeholder="frps 地址（如 192.168.0.10）" />
            <input className="input" type="number" value={frp.server_port} onChange={(e) => setFrp({ ...frp, server_port: Number(e.target.value) || 0 })} placeholder="frps 控制端口（7000）" />
            <input className="input" value={frp.proxy_name} onChange={(e) => setFrp({ ...frp, proxy_name: e.target.value })} placeholder="代理名称（唯一）" />
            <input className="input" type="number" value={frp.remote_port} onChange={(e) => setFrp({ ...frp, remote_port: Number(e.target.value) || 0 })} placeholder="中转机远程端口（17864）" />
            <input className="input" value={frp.local_host} onChange={(e) => setFrp({ ...frp, local_host: e.target.value })} placeholder="本地网关地址（127.0.0.1）" />
            <input className="input" type="number" value={frp.local_port} onChange={(e) => setFrp({ ...frp, local_port: Number(e.target.value) || 0 })} placeholder="本地网关端口（7864）" />
            <input className="input sm:col-span-2" type="password" value={frp.auth_token} onChange={(e) => setFrp({ ...frp, auth_token: e.target.value })} placeholder="frps token（仅保存到本机私有目录，不写日志）" autoComplete="new-password" />
          </div>
          <div className="mt-2 flex flex-wrap gap-4 text-xs text-slate-500 dark:text-zinc-400">
            <label className="flex items-center gap-2">
              <input type="checkbox" checked={frp.tls_enable} onChange={(e) => setFrp({ ...frp, tls_enable: e.target.checked })} />
              FRP 控制连接启用 TLS
            </label>
            <label className="flex items-center gap-2">
              <input type="checkbox" checked={frp.auto_reconnect} onChange={(e) => setFrp({ ...frp, auto_reconnect: e.target.checked })} />
              frpc 进程退出后自动重启
            </label>
          </div>
          <p className="mt-2 text-xs text-slate-400">
            启动前请先在中转机配置 frps 和 Nginx；FRP token 必须与 frps 一致。配置文件生成在应用私有 data/frp 目录，不应提交到仓库。
          </p>
          <div className="mt-3 flex flex-wrap gap-2">
            <button className="btn-secondary" onClick={() => void saveFrp()} disabled={frpBusy}>保存 FRP 配置</button>
            <button className="btn-primary" onClick={() => void startFrp()} disabled={frpBusy || frpStatus?.running === true}>启动 frpc</button>
            <button className="btn-ghost" onClick={() => void stopFrp()} disabled={frpBusy || frpStatus?.running !== true}><Square size={13} />停止</button>
          </div>
          {frpStatus?.message && <p className="mt-2 text-xs text-slate-500 dark:text-zinc-400">{frpStatus.message}</p>}
        </div>

        <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <p className="text-sm font-semibold text-slate-700 dark:text-zinc-200">本机会话存档</p>
              <p className="mt-1 text-xs text-slate-400">
                保存外置 API 的本机对话，账号切换后仍可查看；不读取 Trae/浏览器凭证，不上传远端。
                {conversationCount != null ? ` 当前 ${conversationCount} 个会话。` : ''}
              </p>
            </div>
            <button
              className="btn-outline flex items-center gap-1 text-xs"
              onClick={() => void exportConversations()}
              disabled={archiveBusy}
              title="导出本机 conversations.sqlite3 为 Markdown + JSON"
            >
              <Download size={13} />
              {archiveBusy ? '导出中…' : '导出会话存档'}
            </button>
          </div>
          <div className="mt-3 flex flex-wrap items-center gap-3 border-t border-slate-200 pt-3 text-xs dark:border-zinc-700">
            <label className="flex items-center gap-2 text-slate-600 dark:text-zinc-300">
              <input
                type="checkbox"
                checked={conversationSettings.persist_body}
                onChange={(e) => setConversationSettings({ ...conversationSettings, persist_body: e.target.checked })}
              />
              持久化消息正文
            </label>
            <label className="flex items-center gap-2 text-slate-500 dark:text-zinc-400">
              自动清理天数
              <input
                className="input h-7 w-20 text-xs"
                type="number"
                min={0}
                max={3650}
                value={conversationSettings.retention_days}
                onChange={(e) => setConversationSettings({ ...conversationSettings, retention_days: Number(e.target.value) || 0 })}
                title="0 = 不自动清理"
              />
              <span>0 = 不清理</span>
            </label>
            <button className="btn-ghost ml-auto text-xs" onClick={() => void saveConversationSettings()} disabled={conversationSaving}>
              {conversationSaving ? '保存中…' : '保存会话设置'}
            </button>
          </div>
          {conversationItems.length > 0 && (
            <div className="mt-3 max-h-40 space-y-1 overflow-y-auto border-t border-slate-200 pt-2 dark:border-zinc-700">
              {conversationItems.map((item) => (
                <div key={item.conversation_id} className="flex items-center gap-2 rounded px-2 py-1.5 hover:bg-slate-50 dark:hover:bg-zinc-800/60">
                  <div className="min-w-0 flex-1">
                    <div className="truncate font-mono text-[11px] text-slate-600 dark:text-zinc-300">{item.conversation_id}</div>
                    <div className="text-[10px] text-slate-400">{item.model || '默认模型'} · {item.message_count} 条消息 · {item.pool}</div>
                  </div>
                  <button className="btn-ghost !p-1 text-slate-400 hover:text-red-500" onClick={() => void deleteConversation(item.conversation_id)} title="删除本机会话及消息" aria-label={`删除会话 ${item.conversation_id}`}>
                    <Trash2 size={13} />
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          <div className="mb-2 flex items-center justify-between">
            <p className="font-medium">使用方式 & 配置示例</p>
            <button
              className="btn-ghost flex items-center gap-1 !p-1 text-xs"
              onClick={() => void copyConfigExample()}
              disabled={copying}
              title="复制完整配置示例"
            >
              <Copy size={12} className={copying ? 'animate-pulse' : ''} />
              {copying ? '复制中…' : '复制示例'}
            </button>
          </div>
          <div className="space-y-1.5">
            <div>
              <span className="text-slate-400">接口地址：</span>
              <code className="break-all text-[11px]">http://{gw?.listen_host ?? listenHost}:{gw?.port ?? 7864}/v1</code>
            </div>
            <div>
              <span className="text-slate-400">API Key：</span>
              <code className="text-[11px]">在「API Keys 管理」中创建并复制（网关共享）</code>
            </div>
            <div>
              <span className="text-slate-400">默认模型：</span>
              <code className="text-[11px]">{gw?.default_model ?? '—'}</code>
            </div>
            <div className="pt-1">
              <span className="text-slate-400">其他端点：</span>
            </div>
            <code className="block break-all text-[11px]">
              POST http://{gw?.listen_host ?? listenHost}:{gw?.port ?? 7864}/v1/messages（Anthropic 兼容，x-api-key 鉴权）
            </code>
            <code className="block break-all text-[11px]">
              GET http://{gw?.listen_host ?? listenHost}:{gw?.port ?? 7864}/v1/models（统一模型目录）
            </code>
            <code className="block break-all text-[11px]">
              GET http://{gw?.listen_host ?? listenHost}:{gw?.port ?? 7864}/health
            </code>
            <code className="block break-all text-[11px]">
              POST http://{gw?.listen_host ?? listenHost}:{gw?.port ?? 7864}/v1/assets（参考图/视频，API Key 鉴权）
            </code>
            <p className="pt-1 text-[11px] text-slate-400">
              本机/局域网调用可上传素材到网关；要让 Trae 云端读取，先配置上方素材公开基址，或使用后续原生上传适配器。
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}

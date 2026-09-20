//! 多 API Key 管理与每日配额（T2）+ ck_xxx 子 Key 体系（F-35，批次3）。
//!
//! 数据落盘 `data/api_keys.json`；`daily_limit = 0` 表示不限。
//! 所有 Key 统一在列表中维护（无主/子之分）；未配置任何启用的 Key 时：
//! `auth_disabled = true`（显式关闭鉴权）放行并记为 anonymous，否则拒绝请求（默认）。
//! 每次鉴权命中 Key 即累加当日用量并原子写盘（与 usage.rs 同策略：个人频率低）。
//!
//! F-35 子 Key 体系（对外子 Key 与上游真实凭证分离）：
//! - `ck_` 前缀子 Key（前端 crypto 随机源生成；旧 `sk-` Key 继续兼容）
//! - `allowed_accounts`：限定上游（WB 上游账号 uid 白名单，空 = 不限）
//! - `schedule_mode`：`expire_first`（默认，临期优先）| `dedicated`（专一，固定
//!   `dedicated_account` 或 allowed_accounts 首个）
//! - `daily_stats`：按日请求统计（保留最近 90 天）

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_utils;

/// 数据文件名（位于 data/ 目录）
pub const KEYS_FILE: &str = "api_keys.json";

/// 按日统计保留天数
const DAILY_STATS_CAP: usize = 90;

/// 调度模式：临期优先（默认）
pub const MODE_EXPIRE_FIRST: &str = "expire_first";
/// 调度模式：专一（固定上游账号）
pub const MODE_DEDICATED: &str = "dedicated";

/// 鉴权结果
pub enum KeyCheck {
    /// 命中且已记账，携带约束快照（供 WB 路由层读取上游限定与调度模式）
    Ok(ResolvedKey),
    /// Key 无效或已禁用
    Invalid,
    /// 超出当日配额
    QuotaExceeded { limit: u64 },
    /// Key 已认证，但未被授予当前路由能力。
    CapabilityNotAllowed { capability: String },
}

/// 日配额的兼容错误分类；请求和 Token 共用既有 `daily_quota_exceeded` code，
/// 仅消息按实际额度类型说明单位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaKind {
    Requests,
    Tokens,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTokenReservation {
    pub id: String,
    pub date: String,
    pub amount: u64,
}

/// 请求 guard 持有的 reservation 句柄。句柄本身不携带 Key 明文。
#[derive(Clone, Debug)]
pub struct TokenReservationLease {
    pub(crate) data_dir: PathBuf,
    pub(crate) key_id: String,
    pub(crate) id: String,
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenQuotaError {
    KeyUnavailable,
    Exceeded { limit: u64 },
}

/// Key 可用能力。空 capability 字段表示没有可用能力；字段缺失由 serde 默认成全能力。
pub type KeyCapabilities = Vec<String>;

pub const CAPABILITY_CHAT: &str = "chat";
pub const CAPABILITY_VIDEO: &str = "video";
pub const CAPABILITY_ASSETS: &str = "assets";

const ALL_CAPABILITIES: [&str; 3] = [CAPABILITY_CHAT, CAPABILITY_VIDEO, CAPABILITY_ASSETS];

/// 与全局网关限流器保持一致的 Key 级安全上限。
pub const MAX_KEY_INFLIGHT: usize = 256;
pub const MAX_KEY_VIDEO_JOBS: usize = 256;
pub const MAX_KEY_ASSET_UPLOADS_PER_MINUTE: usize = 10_000;
pub const MAX_KEY_ASSET_BYTES_PER_HOUR: u64 = 10 * 1024 * 1024 * 1024;
pub const MAX_KEY_VIDEO_SUBMISSIONS_PER_MINUTE: usize = 1_000;
pub const MAX_KEY_DAILY_REQUESTS: u64 = 1_000_000;
pub const MAX_KEY_DAILY_TOKENS: u64 = 10_000_000_000;

fn default_capabilities() -> KeyCapabilities {
    ALL_CAPABILITIES.iter().map(|capability| (*capability).to_string()).collect()
}

fn normalize_capabilities(values: KeyCapabilities) -> KeyCapabilities {
    values
        .into_iter()
        .filter(|value| ALL_CAPABILITIES.contains(&value.as_str()))
        .fold(Vec::new(), |mut normalized, value| {
            if !normalized.contains(&value) {
                normalized.push(value);
            }
            normalized
        })
}

fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<KeyCapabilities, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    Ok(normalize_capabilities(values))
}

/// Key 级限流与每日额度覆盖。Option 字段为 None 时继承全局默认值；
/// daily_* 使用 0 表示不限。
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct KeyLimits {
    #[serde(default)]
    pub max_inflight: Option<usize>,
    #[serde(default)]
    pub max_video_jobs: Option<usize>,
    #[serde(default)]
    pub asset_uploads_per_minute: Option<usize>,
    #[serde(default)]
    pub asset_bytes_per_hour: Option<u64>,
    #[serde(default)]
    pub video_submissions_per_minute: Option<usize>,
    #[serde(default)]
    pub daily_requests: u64,
    #[serde(default)]
    pub daily_tokens: u64,
}

impl Default for KeyLimits {
    fn default() -> Self {
        Self {
            max_inflight: None,
            max_video_jobs: None,
            asset_uploads_per_minute: None,
            asset_bytes_per_hour: None,
            video_submissions_per_minute: None,
            daily_requests: 0,
            daily_tokens: 0,
        }
    }
}

impl KeyLimits {
    /// 将输入限制到安全范围；继承型 Option 字段的 0 会钳制为最小安全值 1。
    pub fn normalize(&mut self) {
        self.max_inflight = normalize_optional(self.max_inflight, MAX_KEY_INFLIGHT);
        self.max_video_jobs = normalize_optional(self.max_video_jobs, MAX_KEY_VIDEO_JOBS);
        self.asset_uploads_per_minute =
            normalize_optional(self.asset_uploads_per_minute, MAX_KEY_ASSET_UPLOADS_PER_MINUTE);
        self.asset_bytes_per_hour = normalize_optional(self.asset_bytes_per_hour, MAX_KEY_ASSET_BYTES_PER_HOUR);
        self.video_submissions_per_minute = normalize_optional(
            self.video_submissions_per_minute,
            MAX_KEY_VIDEO_SUBMISSIONS_PER_MINUTE,
        );
        self.daily_requests = self.daily_requests.min(MAX_KEY_DAILY_REQUESTS);
        self.daily_tokens = self.daily_tokens.min(MAX_KEY_DAILY_TOKENS);
    }

    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }
}

fn normalize_optional<T>(value: Option<T>, max: T) -> Option<T>
where
    T: Ord + From<u8>,
{
    value.map(|value| value.max(T::from(1)).min(max))
}

#[derive(Deserialize)]
struct KeyLimitsWire {
    #[serde(default)]
    max_inflight: Option<usize>,
    #[serde(default)]
    max_video_jobs: Option<usize>,
    #[serde(default)]
    asset_uploads_per_minute: Option<usize>,
    #[serde(default)]
    asset_bytes_per_hour: Option<u64>,
    #[serde(default)]
    video_submissions_per_minute: Option<usize>,
    #[serde(default)]
    daily_requests: u64,
    #[serde(default)]
    daily_tokens: u64,
}

impl<'de> Deserialize<'de> for KeyLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = KeyLimitsWire::deserialize(deserializer)?;
        Ok(Self {
            max_inflight: wire.max_inflight,
            max_video_jobs: wire.max_video_jobs,
            asset_uploads_per_minute: wire.asset_uploads_per_minute,
            asset_bytes_per_hour: wire.asset_bytes_per_hour,
            video_submissions_per_minute: wire.video_submissions_per_minute,
            daily_requests: wire.daily_requests,
            daily_tokens: wire.daily_tokens,
        }
        .normalized())
    }
}

/// 鉴权通过后的 Key 约束快照（F-35）
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedKey {
    pub id: String,
    /// 限定上游账号 uid 白名单；空 = 不限
    pub allowed_accounts: Vec<String>,
    /// expire_first | dedicated
    pub schedule_mode: String,
    /// 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）
    pub dedicated_account: String,
    pub limits: KeyLimits,
    pub capabilities: KeyCapabilities,
}

/// 子 Key 按日统计项
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KeyDailyStat {
    pub date: String,
    pub requests: u64,
    /// 已知上游用量；缺失 token_usage 时保持 0，不做估算。
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

/// API Key 条目
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKeyEntry {
    /// 唯一标识（前端生成）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 实际 Key 值（ck_xxx / sk-...）
    pub key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 每日请求配额，0 = 不限
    #[serde(default)]
    pub daily_limit: u64,
    /// 创建时间（unix 秒）
    #[serde(default)]
    pub created_at: u64,
    /// 当日用量记账日期（YYYY-MM-DD）
    #[serde(default)]
    pub used_date: String,
    /// 当日已用请求数
    #[serde(default)]
    pub used_today: u64,
    // ── F-35 子 Key 体系（批次3；serde default 兼容旧文件）──
    /// 限定上游账号 uid 白名单（WB 上游 uid；空 = 不限）
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    /// 调度模式：expire_first（默认）| dedicated
    #[serde(default)]
    pub schedule_mode: String,
    /// 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）
    #[serde(default)]
    pub dedicated_account: String,
    /// 按日请求统计（升序，保留最近 90 天）
    #[serde(default)]
    pub daily_stats: Vec<KeyDailyStat>,
    /// 当日尚未结算的保守 Token 预留；仅在 Key 锁内读改写。
    #[serde(default)]
    pub token_reservations: Vec<KeyTokenReservation>,
    /// Key 级限流与每日额度覆盖；缺失时继承全局默认。
    #[serde(default)]
    pub limits: KeyLimits,
    /// 允许的能力；缺失时兼容旧 Key，默认允许 chat/video/assets。
    #[serde(default = "default_capabilities", deserialize_with = "deserialize_capabilities")]
    pub capabilities: KeyCapabilities,
}

fn default_true() -> bool {
    true
}

/// 数据文件根结构
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKeysFile {
    #[serde(default)]
    pub keys: Vec<ApiKeyEntry>,
    /// 显式关闭鉴权：仅当没有任何启用 Key 时生效（true = 无 Key 放行；默认 false = 无 Key 拒绝）
    #[serde(default)]
    pub auth_disabled: bool,
}

impl Default for ApiKeysFile {
    fn default() -> Self {
        Self { keys: Vec::new(), auth_disabled: false }
    }
}

impl ApiKeyEntry {
    pub fn schedule_mode(&self) -> &str {
        if self.schedule_mode.is_empty() {
            MODE_EXPIRE_FIRST
        } else {
            &self.schedule_mode
        }
    }

    fn normalize_policy(&mut self) {
        self.limits.normalize();
        self.capabilities = normalize_capabilities(std::mem::take(&mut self.capabilities));
    }
}

/// 当日是否仍有配额；跨天自动重置计数
fn quota_left(e: &mut ApiKeyEntry, today: &str) -> Result<(), u64> {
    if e.used_date != today {
        e.used_date = today.to_string();
        e.used_today = 0;
    }
    if e.daily_limit > 0 && e.used_today >= e.daily_limit {
        return Err(e.daily_limit);
    }
    if e.limits.daily_tokens > 0 {
        let used_tokens = e
            .daily_stats
            .iter()
            .find(|stat| stat.date == today)
            .map(|stat| stat.prompt_tokens.saturating_add(stat.completion_tokens))
            .unwrap_or(0);
        if used_tokens >= e.limits.daily_tokens {
            return Err(e.limits.daily_tokens);
        }
    }
    Ok(())
}

/// 按日统计记账：当日项 find-or-insert +1，cap 90 天
fn bump_daily_stats(e: &mut ApiKeyEntry, today: &str) {
    let need_new = e
        .daily_stats
        .last()
        .map(|s| s.date.as_str() != today)
        .unwrap_or(true);
    if need_new {
        e.daily_stats.push(KeyDailyStat {
            date: today.to_string(),
            requests: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
        });
        if e.daily_stats.len() > DAILY_STATS_CAP {
            let drop = e.daily_stats.len() - DAILY_STATS_CAP;
            e.daily_stats.drain(0..drop);
        }
    }
    if let Some(s) = e.daily_stats.last_mut() {
        s.requests = s.requests.saturating_add(1);
    }
}

fn add_daily_tokens(e: &mut ApiKeyEntry, today: &str, prompt_tokens: u64, completion_tokens: u64) {
    let need_new = e
        .daily_stats
        .last()
        .map(|s| s.date.as_str() != today)
        .unwrap_or(true);
    if need_new {
        e.daily_stats.push(KeyDailyStat {
            date: today.to_string(),
            requests: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
        });
        if e.daily_stats.len() > DAILY_STATS_CAP {
            let drop = e.daily_stats.len() - DAILY_STATS_CAP;
            e.daily_stats.drain(0..drop);
        }
    }
    if let Some(stat) = e.daily_stats.last_mut() {
        stat.prompt_tokens = stat.prompt_tokens.saturating_add(prompt_tokens);
        stat.completion_tokens = stat.completion_tokens.saturating_add(completion_tokens);
    }
}

impl ApiKeysFile {
    fn normalize_policies(&mut self) {
        for key in &mut self.keys {
            key.normalize_policy();
        }
    }

    /// 按呈现的 Key 校验并记账（命中即 +1 + 按日统计）。调用方负责把结果写盘。
    pub fn verify_and_consume(&mut self, presented: &str, today: &str) -> KeyCheck {
        self.verify_and_consume_with_capability(presented, today, None)
    }

    /// 按呈现的 Key 校验、先检查能力再记账。
    /// 能力拒绝必须发生在每日请求计数和配额检查之前，避免 403 被计量。
    pub fn verify_and_consume_for_capability(
        &mut self,
        presented: &str,
        today: &str,
        capability: &str,
    ) -> KeyCheck {
        self.verify_and_consume_with_capability(presented, today, Some(capability))
    }

    fn verify_and_consume_with_capability(
        &mut self,
        presented: &str,
        today: &str,
        capability: Option<&str>,
    ) -> KeyCheck {
        // 常量时间比较（审查 P2-3）：对两侧求 sha256 再比对，避免逐字节提前返回泄露前缀匹配长度
        let digest = |s: &str| {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(s.as_bytes());
            h.finalize()
        };
        let pd = digest(presented);
        let Some(e) = self
            .keys
            .iter_mut()
            .find(|k| k.enabled && digest(&k.key) == pd)
        else {
            return KeyCheck::Invalid;
        };
        e.normalize_policy();
        if let Some(capability) = capability {
            if !e.capabilities.iter().any(|value| value == capability) {
                return KeyCheck::CapabilityNotAllowed {
                    capability: capability.to_string(),
                };
            }
        }
        if let Err(limit) = quota_left(e, today) {
            return KeyCheck::QuotaExceeded { limit };
        }
        e.used_today += 1;
        bump_daily_stats(e, today);
        KeyCheck::Ok(ResolvedKey {
            id: e.id.clone(),
            allowed_accounts: e.allowed_accounts.clone(),
            schedule_mode: e.schedule_mode().to_string(),
            dedicated_account: e.dedicated_account.clone(),
            limits: e.limits.clone(),
            capabilities: e.capabilities.clone(),
        })
    }

    /// 是否存在启用的子 Key（用于判断是否需要鉴权）
    pub fn has_enabled(&self) -> bool {
        self.keys.iter().any(|k| k.enabled)
    }
}

/// 按 Key 条目 id 解析约束快照（WB 路由层每流程调用一次；Key 不存在返回 None）
pub fn constraints_for(data_dir: &Path, key_id: &str) -> Option<ResolvedKey> {
    let f: ApiKeysFile = load(data_dir);
    f.keys.iter().find(|k| k.id == key_id).map(|e| ResolvedKey {
        id: e.id.clone(),
        allowed_accounts: e.allowed_accounts.clone(),
        schedule_mode: e.schedule_mode().to_string(),
        dedicated_account: e.dedicated_account.clone(),
        limits: e.limits.clone(),
        capabilities: e.capabilities.clone(),
    })
}

/// 查询 Key 当日已知 token 用量是否已达到额度；只读，不消费请求额度。
pub fn daily_token_quota(data_dir: &Path, key_id: &str, today: &str) -> Result<(), u64> {
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = load(data_dir);
    let Some(entry) = file.keys.iter().find(|entry| entry.id == key_id) else {
        return Ok(());
    };
    if entry.limits.daily_tokens == 0 {
        return Ok(());
    }
    let used_tokens = entry
        .daily_stats
        .iter()
        .find(|stat| stat.date == today)
        .map(|stat| stat.prompt_tokens.saturating_add(stat.completion_tokens))
        .unwrap_or(0);
    if used_tokens >= entry.limits.daily_tokens {
        Err(entry.limits.daily_tokens)
    } else {
        Ok(())
    }
}

/// 数据文件路径：data_dir/data/api_keys.json
pub fn keys_path(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("data");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(KEYS_FILE)
}

/// 读盘
pub fn load(data_dir: &Path) -> ApiKeysFile {
    fs_utils::read_json(&keys_path(data_dir))
}

/// 原子写盘
pub fn save(data_dir: &Path, f: &ApiKeysFile) {
    let mut normalized = f.clone();
    normalized.normalize_policies();
    let _ = fs_utils::write_json(&keys_path(data_dir), &normalized);
}

/// 进程级写锁（审查 P1-2）：api_keys.json 的「读-改-写」（verify 记账 + save）必须
/// 原子完成，否则并发请求互相覆盖 used_today/daily_stats——配额可被穿透、统计少记。
static KEYS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 在已有 Key 锁内更新已知 token 用量；请求数由鉴权路径单独记账。
/// token_usage 缺失时调用方传入 0，因而不会伪造 token。
pub fn record_token_usage_locked(
    data_dir: &Path,
    key_id: &str,
    today: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) {
    if key_id == "anonymous" || (prompt_tokens == 0 && completion_tokens == 0) {
        return;
    }
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load(data_dir);
    let Some(entry) = file.keys.iter_mut().find(|entry| entry.id == key_id) else {
        return;
    };
    entry.normalize_policy();
    add_daily_tokens(entry, today, prompt_tokens, completion_tokens);
    save(data_dir, &file);
}

/// 为一个 legacy text request 保守预留当日剩余 Token 容量。
///
/// 认证阶段无法可靠知道上游最终 usage，因此有限 Token Key 每次只允许一个
/// 尚未结算的请求占用“全部剩余容量”。已知 usage 结算时释放未使用部分；失败、
/// 未知 usage 则由 guard 释放，避免把未知用量伪造为已知 Token。
pub fn reserve_token_quota(
    data_dir: &Path,
    key_id: &str,
    today: &str,
    daily_tokens: u64,
) -> Result<Option<TokenReservationLease>, TokenQuotaError> {
    if key_id == "anonymous" || daily_tokens == 0 {
        return Ok(None);
    }
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load(data_dir);
    let Some(entry) = file.keys.iter_mut().find(|entry| entry.id == key_id) else {
        return Err(TokenQuotaError::KeyUnavailable);
    };
    entry
        .token_reservations
        .retain(|reservation| reservation.date == today);
    let used_tokens = entry
        .daily_stats
        .iter()
        .find(|stat| stat.date == today)
        .map(|stat| stat.prompt_tokens.saturating_add(stat.completion_tokens))
        .unwrap_or(0);
    let reserved_tokens = entry
        .token_reservations
        .iter()
        .map(|reservation| reservation.amount)
        .sum::<u64>();
    let occupied = used_tokens.saturating_add(reserved_tokens);
    if occupied >= daily_tokens {
        return Err(TokenQuotaError::Exceeded { limit: daily_tokens });
    }
    let amount = daily_tokens.saturating_sub(occupied);
    let id = format!("key-token-reservation-{:032x}", rand::random::<u128>());
    entry.token_reservations.push(KeyTokenReservation {
        id: id.clone(),
        date: today.to_string(),
        amount,
    });
    save(data_dir, &file);
    Ok(Some(TokenReservationLease {
        data_dir: data_dir.to_path_buf(),
        key_id: key_id.to_string(),
        id,
        amount,
    }))
}

/// 结算一个 reservation；Token 统计和 reservation 删除在同一 Key 锁内完成。
pub fn settle_token_reservation(
    lease: &TokenReservationLease,
    today: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) {
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load(&lease.data_dir);
    let Some(entry) = file.keys.iter_mut().find(|entry| entry.id == lease.key_id) else {
        return;
    };
    let Some(index) = entry
        .token_reservations
        .iter()
        .position(|reservation| reservation.id == lease.id)
    else {
        return;
    };
    entry.token_reservations.remove(index);
    add_daily_tokens(entry, today, prompt_tokens, completion_tokens);
    save(&lease.data_dir, &file);
}

/// 失败/未知 usage 的保守策略：仅删除本句柄，释放容量供下一请求使用。
pub fn release_token_reservation(lease: &TokenReservationLease) {
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load(&lease.data_dir);
    let Some(entry) = file.keys.iter_mut().find(|entry| entry.id == lease.key_id) else {
        return;
    };
    let before = entry.token_reservations.len();
    entry
        .token_reservations
        .retain(|reservation| reservation.id != lease.id);
    if entry.token_reservations.len() != before {
        save(&lease.data_dir, &file);
    }
}

/// 序列化对比：verify 后文件是否发生变化（P1 修复5a）。
/// 无变化（Invalid 等只读路径）跳过写盘，消除鉴权热路径的无效磁盘写
fn keys_file_changed(before: &[u8], f: &ApiKeysFile) -> bool {
    serde_json::to_vec(f).map(|b| b != before).unwrap_or(true)
}

/// 鉴权记账原子操作：锁内 load → verify_and_consume → 有变化才 save。
/// auth 中间件每请求调用本函数，禁止绕开锁直接 load+save。
pub fn verify_and_consume_locked(data_dir: &Path, presented: &str, today: &str) -> KeyCheck {
    verify_and_consume_locked_for_capability(data_dir, presented, today, None)
}

/// 锁内完成鉴权、能力校验和请求记账；能力不允许时不改变每日统计。
pub fn verify_and_consume_locked_for_capability(
    data_dir: &Path,
    presented: &str,
    today: &str,
    capability: Option<&str>,
) -> KeyCheck {
    let _guard = KEYS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match aiwork_core::CoreStore::legacy_key_is_disabled(data_dir, presented) {
        Ok(false) => {}
        Ok(true) | Err(_) => return KeyCheck::Invalid,
    }
    let mut f = load(data_dir);
    let before = serde_json::to_vec(&f).unwrap_or_default();
    let r = f.verify_and_consume_with_capability(presented, today, capability);
    if keys_file_changed(&before, &f) {
        save(data_dir, &f);
    }
    r
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use aiwork_core::{
        CoreStore, LegacyMigrationBatch, LegacyMigrationKey, NewUser, Principal, UserRole,
    };
    use serde_json::json;

    use super::*;

    fn entry(id: &str, key: &str, enabled: bool, limit: u64) -> ApiKeyEntry {
        ApiKeyEntry {
            id: id.into(),
            name: id.into(),
            key: key.into(),
            enabled,
            daily_limit: limit,
            created_at: 0,
            used_date: String::new(),
            used_today: 0,
            allowed_accounts: vec![],
            schedule_mode: String::new(),
            dedicated_account: String::new(),
            daily_stats: vec![],
            token_reservations: vec![],
            limits: KeyLimits::default(),
            capabilities: default_capabilities(),
        }
    }

    #[test]
    fn legacy_key_json_defaults_to_all_capabilities_and_inherited_limits() {
        let legacy = json!({
            "keys": [{
                "id": "legacy-id",
                "name": "legacy",
                "key": "",
                "enabled": true,
                "daily_limit": 0,
                "created_at": 0,
                "used_date": "",
                "used_today": 0,
                "allowed_accounts": [],
                "schedule_mode": "",
                "dedicated_account": "",
                "daily_stats": []
            }],
            "auth_disabled": false
        });

        let file: ApiKeysFile = serde_json::from_value(legacy).unwrap();
        let key = &file.keys[0];
        assert_eq!(key.capabilities, default_capabilities());
        assert!(key.limits.max_inflight.is_none());
        assert!(key.limits.asset_uploads_per_minute.is_none());
        assert!(key.limits.asset_bytes_per_hour.is_none());
        assert!(key.limits.video_submissions_per_minute.is_none());
        assert!(key.limits.max_video_jobs.is_none());
        assert_eq!(key.limits.daily_requests, 0);
        assert_eq!(key.limits.daily_tokens, 0);
    }

    #[test]
    fn non_empty_legacy_key_still_authenticates_with_default_policy() {
        let legacy = json!({
            "keys": [{
                "id": "legacy-id",
                "name": "legacy",
                "key": "fixture-key",
                "enabled": true,
                "daily_limit": 0,
                "created_at": 0,
                "used_date": "",
                "used_today": 0,
                "allowed_accounts": [],
                "schedule_mode": "",
                "dedicated_account": "",
                "daily_stats": []
            }],
            "auth_disabled": false
        });

        let mut file: ApiKeysFile = serde_json::from_value(legacy).unwrap();
        let KeyCheck::Ok(resolved) = file.verify_and_consume("fixture-key", "2026-09-20") else {
            panic!("expected legacy key verification to succeed");
        };
        assert_eq!(resolved.id, "legacy-id");
        assert_eq!(resolved.capabilities, default_capabilities());
        assert!(resolved.limits.max_video_jobs.is_none());
    }

    #[test]
    fn explicit_empty_capabilities_remain_empty_after_save_and_load() {
        let mut encoded = serde_json::to_value(ApiKeysFile {
            keys: vec![entry("k1", "", true, 0)],
            auth_disabled: false,
        })
        .unwrap();
        encoded["keys"][0]["capabilities"] = json!([]);
        let file: ApiKeysFile = serde_json::from_value(encoded).unwrap();
        assert!(file.keys[0].capabilities.is_empty());
        let dir = std::env::temp_dir().join(format!(
            "twa_keys_empty_capabilities_{}_{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        save(&dir, &file);
        let loaded = load(&dir);
        assert!(loaded.keys[0].capabilities.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capabilities_are_normalized_on_persistence() {
        let mut file = ApiKeysFile {
            keys: vec![entry("k1", "", true, 0)],
            auth_disabled: false,
        };
        file.keys[0].capabilities = vec!["chat".into(), "unknown".into(), "chat".into()];
        let dir = std::env::temp_dir().join(format!(
            "twa_keys_capabilities_{}_{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        save(&dir, &file);
        let loaded = load(&dir);
        assert_eq!(loaded.keys[0].capabilities, vec!["chat"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_limits_normalize_negative_or_oversized_values() {
        assert!(serde_json::from_value::<KeyLimits>(json!({
            "max_inflight": -1
        }))
        .is_err());

        let mut limits = KeyLimits {
            max_inflight: Some(usize::MAX),
            max_video_jobs: Some(usize::MAX),
            asset_uploads_per_minute: Some(usize::MAX),
            asset_bytes_per_hour: Some(u64::MAX),
            video_submissions_per_minute: Some(usize::MAX),
            daily_requests: u64::MAX,
            daily_tokens: u64::MAX,
        };
        limits.normalize();
        assert_eq!(limits.max_inflight, Some(MAX_KEY_INFLIGHT));
        assert_eq!(limits.max_video_jobs, Some(MAX_KEY_VIDEO_JOBS));
        assert_eq!(
            limits.asset_uploads_per_minute,
            Some(MAX_KEY_ASSET_UPLOADS_PER_MINUTE)
        );
        assert_eq!(limits.asset_bytes_per_hour, Some(MAX_KEY_ASSET_BYTES_PER_HOUR));
        assert_eq!(
            limits.video_submissions_per_minute,
            Some(MAX_KEY_VIDEO_SUBMISSIONS_PER_MINUTE)
        );
        assert_eq!(limits.daily_requests, MAX_KEY_DAILY_REQUESTS);
        assert_eq!(limits.daily_tokens, MAX_KEY_DAILY_TOKENS);
    }

    #[test]
    fn resolved_key_carries_limits_and_capabilities_without_key_material() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "fixture-key", true, 0)],
            auth_disabled: false,
        };
        f.keys[0].limits = KeyLimits {
            max_inflight: Some(2),
            max_video_jobs: Some(4),
            asset_uploads_per_minute: None,
            asset_bytes_per_hour: Some(4096),
            video_submissions_per_minute: Some(3),
            daily_requests: 100,
            daily_tokens: 10_000,
        };
        f.keys[0].capabilities = vec!["chat".into(), "assets".into()];

        let KeyCheck::Ok(resolved) = f.verify_and_consume("fixture-key", "2026-09-20") else {
            panic!("expected key verification to succeed");
        };
        assert_eq!(resolved.id, "k1");
        assert_eq!(resolved.limits, f.keys[0].limits);
        assert_eq!(resolved.limits.max_video_jobs, Some(4));
        assert_eq!(resolved.capabilities, f.keys[0].capabilities);
        let serialized = serde_json::to_string(&resolved).unwrap();
        assert!(!serialized.contains("\"key\""));
    }

    #[test]
    fn disabled_capability_is_rejected_before_daily_request_consumption() {
        let mut file = ApiKeysFile {
            keys: vec![entry("k1", "fixture-capability", true, 1)],
            auth_disabled: false,
        };
        file.keys[0].capabilities = vec![CAPABILITY_CHAT.into()];

        assert!(matches!(
            file.verify_and_consume_for_capability(
                "fixture-capability",
                "2026-09-20",
                CAPABILITY_ASSETS,
            ),
            KeyCheck::CapabilityNotAllowed { .. }
        ));
        assert_eq!(file.keys[0].used_today, 0);
        assert!(matches!(
            file.verify_and_consume_for_capability(
                "fixture-capability",
                "2026-09-20",
                CAPABILITY_CHAT,
            ),
            KeyCheck::Ok(_)
        ));
        assert_eq!(file.keys[0].used_today, 1);
    }

    #[test]
    fn verify_matches_enabled_key_and_counts() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "2026-09-09"), KeyCheck::Ok(r) if r.id == "k1"));
        assert_eq!(f.keys[0].used_today, 1);
        assert_eq!(f.keys[0].used_date, "2026-09-09");
        assert_eq!(f.keys[0].daily_stats.len(), 1);
        assert_eq!(f.keys[0].daily_stats[0].requests, 1);
    }

    #[test]
    fn verify_rejects_disabled_or_unknown() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", false, 0), entry("k2", "ck-b", true, 0)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "d"), KeyCheck::Invalid));
        assert!(matches!(f.verify_and_consume("ck-c", "d"), KeyCheck::Invalid));
    }

    #[test]
    fn quota_blocks_at_limit_and_resets_next_day() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 2)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(f.verify_and_consume("ck-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(
            f.verify_and_consume("ck-a", "d1"),
            KeyCheck::QuotaExceeded { limit: 2 }
        ));
        // 跨天重置
        assert!(matches!(f.verify_and_consume("ck-a", "d2"), KeyCheck::Ok(_)));
        assert_eq!(f.keys[0].used_today, 1);
        // 按日统计：两天各一项
        assert_eq!(f.keys[0].daily_stats.len(), 2);
        assert_eq!(f.keys[0].daily_stats[1].requests, 1);
    }

    #[test]
    fn token_reservation_blocks_a_second_request_and_settles_known_usage() {
        let dir = std::path::PathBuf::from(r"D:\gpt").join(format!(
            "twa-keys-token-reservation-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut key = entry("k1", "fixture-token-reservation", true, 0);
        key.limits.daily_tokens = 10;
        save(
            &dir,
            &ApiKeysFile {
                keys: vec![key],
                auth_disabled: false,
            },
        );

        let first = reserve_token_quota(&dir, "k1", "day-1", 10)
            .expect("first reservation should be accepted")
            .expect("a finite token limit should create a reservation");
        assert_eq!(first.amount, 10);
        assert!(matches!(
            reserve_token_quota(&dir, "k1", "day-1", 10),
            Err(TokenQuotaError::Exceeded { limit: 10 })
        ));

        settle_token_reservation(&first, "day-1", 6, 4);
        let loaded = load(&dir);
        assert!(loaded.keys[0].token_reservations.is_empty());
        assert_eq!(
            loaded.keys[0].daily_stats[0].prompt_tokens
                + loaded.keys[0].daily_stats[0].completion_tokens,
            10
        );
        assert!(matches!(
            reserve_token_quota(&dir, "k1", "day-1", 10),
            Err(TokenQuotaError::Exceeded { limit: 10 })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_reservation_releases_unknown_usage_without_leaking_capacity() {
        let dir = std::path::PathBuf::from(r"D:\gpt").join(format!(
            "twa-keys-token-release-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut key = entry("k1", "fixture-token-release", true, 0);
        key.limits.daily_tokens = 10;
        save(
            &dir,
            &ApiKeysFile {
                keys: vec![key],
                auth_disabled: false,
            },
        );

        let first = reserve_token_quota(&dir, "k1", "day-1", 10)
            .unwrap()
            .unwrap();
        release_token_reservation(&first);
        let second = reserve_token_quota(&dir, "k1", "day-1", 10)
            .expect("released reservation should make capacity available")
            .expect("the second request should reserve the full remaining budget");
        assert_ne!(first.id, second.id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_token_reservations_allow_only_one_request_for_remaining_budget() {
        let dir = std::path::PathBuf::from(r"D:\gpt").join(format!(
            "twa-keys-token-reservation-concurrent-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut key = entry("k1", "fixture-token-reservation-concurrent", true, 0);
        key.limits.daily_tokens = 10;
        save(
            &dir,
            &ApiKeysFile {
                keys: vec![key],
                auth_disabled: false,
            },
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let dir = dir.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_token_quota(&dir, "k1", "day-1", 10)
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation worker should not panic"))
            .collect();

        let mut accepted = Vec::new();
        let mut rejected = 0;
        for result in results {
            match result {
                Ok(Some(lease)) => accepted.push(lease),
                Ok(None) => panic!("a finite token limit should create a reservation"),
                Err(TokenQuotaError::Exceeded { limit }) => {
                    assert_eq!(limit, 10);
                    rejected += 1;
                }
                Err(error) => panic!("reservation should only be rejected by the token quota: {error:?}"),
            }
        }
        assert_eq!(accepted.len(), 1, "only one request may reserve the remaining budget");
        assert_eq!(rejected, 1, "the other request must hit the token quota");
        release_token_reservation(&accepted[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daily_stats_capped_at_90() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        for i in 0..120 {
            let date = format!("d{i}");
            let _ = f.verify_and_consume("ck-a", &date);
        }
        assert_eq!(f.keys[0].daily_stats.len(), 90);
        assert_eq!(f.keys[0].daily_stats[0].date, "d30");
    }

    #[test]
    fn resolved_key_defaults_and_constraints() {
        let mut e = entry("k1", "ck-a", true, 0);
        assert_eq!(e.schedule_mode(), MODE_EXPIRE_FIRST);
        e.schedule_mode = MODE_DEDICATED.into();
        e.allowed_accounts = vec!["wb-1".into()];
        assert_eq!(e.schedule_mode(), MODE_DEDICATED);
    }

    #[test]
    fn roundtrip_with_defaults() {
        let dir = std::env::temp_dir().join(format!("twa_keys_test_{}", std::process::id()));
        let mut f = ApiKeysFile::default();
        let mut e = entry("k1", "ck-x", true, 5);
        e.allowed_accounts = vec!["wb-9".into()];
        e.schedule_mode = MODE_DEDICATED.into();
        e.dedicated_account = "wb-9".into();
        f.keys.push(e);
        save(&dir, &f);
        let loaded = load(&dir);
        // 非空文件原样读盘
        assert_eq!(loaded.keys.len(), 1);
        assert_eq!(loaded.keys[0].daily_limit, 5);
        assert_eq!(loaded.keys[0].allowed_accounts, vec!["wb-9"]);
        assert_eq!(loaded.keys[0].schedule_mode(), MODE_DEDICATED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== P1 修复5a：无变化跳过写盘 ====================

    #[test]
    fn changed_detects_consume_but_not_invalid() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        let before = serde_json::to_vec(&f).unwrap();
        assert!(!keys_file_changed(&before, &f), "未变更不应触发写盘");
        let _ = f.verify_and_consume("ck-a", "d1");
        assert!(keys_file_changed(&before, &f), "记账后应触发写盘");
    }

    #[test]
    fn locked_verify_skips_write_when_unchanged() {
        // Invalid 路径不改写文件；命中记账路径正常落盘
        let dir = std::env::temp_dir().join(format!("twa_keys_locked_{}", std::process::id()));
        let f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        save(&dir, &f);
        let before = std::fs::read(keys_path(&dir)).unwrap();
        assert!(matches!(
            verify_and_consume_locked(&dir, "ck-wrong", "d1"),
            KeyCheck::Invalid
        ));
        let after = std::fs::read(keys_path(&dir)).unwrap();
        assert_eq!(before, after, "Invalid 路径不应改写文件");
        // 命中记账：文件应更新
        assert!(matches!(
            verify_and_consume_locked(&dir, "ck-a", "d1"),
            KeyCheck::Ok(_)
        ));
        let updated = load(&dir);
        assert_eq!(updated.keys[0].used_today, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrated_legacy_key_is_rejected_by_locked_legacy_auth() {
        let dir = std::env::temp_dir().join(format!("twa_keys_migration_{}", rand::random::<u64>()));
        let store = CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        store
            .create_user(
                NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin },
                "bootstrap",
            )
            .unwrap();
        store
            .create_user(
                NewUser { id: "user".into(), name: "User".into(), role: UserRole::User },
                "admin",
            )
            .unwrap();
        let admin_key = store
            .issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
            .unwrap();
        store
            .apply_legacy_migration(LegacyMigrationBatch {
                migration_id: "migration-1".into(),
                actor: Principal {
                    user_id: "admin".into(),
                    key_id: admin_key.id,
                    scopes: BTreeSet::from(["admin:*".into()]),
                },
                reason: "test migration".into(),
                scopes: BTreeSet::new(),
                source_hashes: BTreeMap::from([("api_keys.json".into(), "hash".into())]),
                keys: vec![LegacyMigrationKey {
                    legacy_key_id: "legacy-1".into(),
                    legacy_key: "legacy-secret".into(),
                    user_id: "user".into(),
                }],
                assets: vec![],
                jobs: vec![],
                observations: vec![],
            })
            .unwrap();
        save(&dir, &ApiKeysFile {
            keys: vec![entry("legacy-1", "legacy-secret", true, 0)],
            auth_disabled: false,
        });

        assert!(matches!(
            verify_and_consume_locked(&dir, "legacy-secret", "2026-09-19"),
            KeyCheck::Invalid
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

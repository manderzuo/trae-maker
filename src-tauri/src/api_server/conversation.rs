//! 外置 API 的可选会话连续性（阶段 C 第一批）。
//!
//! 默认网关仍是无状态的：只有请求携带 `conversation_id` 或
//! `X-Conversation-ID` 时才会创建本地会话库。消息正文只保存在本机
//! `%APPDATA%\\AIWorkAssistant\\data\\conversations.sqlite3`，不进入 API 日志。
//! 上游账号切换时继续使用同一个本地会话键，但不会把账号 A 的云端会话
//! ID 伪装给账号 B；每个账号会得到独立、稳定的上游会话 ID。

use std::path::{Path, PathBuf};

use axum::http::HeaderMap;
use rusqlite::{params, Connection};
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub model: String,
    pub pool: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationExportResult {
    pub conversations: usize,
    pub messages: usize,
    pub json_path: String,
    pub markdown_path: String,
}

const DB_FILE: &str = "conversations.sqlite3";
const SETTINGS_FILE: &str = "conversation_settings.json";
const MAX_ID_LEN: usize = 200;
const MAX_UPSTREAM_CONTEXT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationSettings {
    /// 是否持久化消息正文。关闭后仍接受 conversation_id，但只在当前请求内处理历史。
    pub persist_body: bool,
    /// 自动清理更新时间早于 N 天的会话；0 = 不自动清理。
    pub retention_days: u32,
}

impl Default for ConversationSettings {
    fn default() -> Self {
        Self { persist_body: true, retention_days: 0 }
    }
}

fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("data").join(DB_FILE)
}

fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("data").join(SETTINGS_FILE)
}

pub fn load_settings(data_dir: &Path) -> ConversationSettings {
    std::fs::read(settings_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_settings(data_dir: &Path, mut settings: ConversationSettings) -> Result<ConversationSettings, String> {
    settings.retention_days = settings.retention_days.min(3650);
    let path = settings_path(data_dir);
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| format!("创建会话配置目录失败：{e}"))?; }
    let bytes = serde_json::to_vec_pretty(&settings).map_err(|e| format!("序列化会话配置失败：{e}"))?;
    std::fs::write(path, bytes).map_err(|e| format!("保存会话配置失败：{e}"))?;
    if settings.retention_days > 0 { prune(data_dir, settings.retention_days); }
    Ok(settings)
}

fn prune(data_dir: &Path, retention_days: u32) {
    let cutoff = now_ts().saturating_sub((retention_days as i64).saturating_mul(86_400));
    let conn = match open_db(data_dir) { Ok(c) => c, Err(_) => return };
    let _ = conn.execute("DELETE FROM conversation_messages WHERE conversation_id IN (SELECT conversation_id FROM conversations WHERE updated_at < ?1)", params![cutoff]);
    let _ = conn.execute("DELETE FROM conversations WHERE updated_at < ?1", params![cutoff]);
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_db(data_dir: &Path) -> rusqlite::Result<Connection> {
    let path = db_path(data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS conversations (
             conversation_id TEXT PRIMARY KEY,
             model TEXT NOT NULL DEFAULT '',
             pool TEXT NOT NULL DEFAULT 'trae',
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS conversation_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             conversation_id TEXT NOT NULL,
             seq INTEGER NOT NULL,
             role TEXT NOT NULL,
             message_json TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             FOREIGN KEY(conversation_id) REFERENCES conversations(conversation_id)
         );
         CREATE INDEX IF NOT EXISTS idx_conversation_messages
             ON conversation_messages(conversation_id, seq);",
    )?;
    Ok(conn)
}

/// 列出本机外置 API 会话。仅读取助手自己的 conversations.sqlite3，
/// 不访问 Trae/浏览器凭证，也不向任何远端发送数据。
pub fn list_summaries(data_dir: &Path) -> Result<Vec<ConversationSummary>, String> {
    let settings = load_settings(data_dir);
    if settings.retention_days > 0 { prune(data_dir, settings.retention_days); }
    let conn = open_db(data_dir).map_err(|e| format!("打开会话库失败：{e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT c.conversation_id, c.model, c.pool, c.created_at, c.updated_at,
                    COUNT(m.id) AS message_count
             FROM conversations c
             LEFT JOIN conversation_messages m ON m.conversation_id = c.conversation_id
             GROUP BY c.conversation_id, c.model, c.pool, c.created_at, c.updated_at
             ORDER BY c.updated_at DESC, c.conversation_id ASC",
        )
        .map_err(|e| format!("读取会话列表失败：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ConversationSummary {
                conversation_id: row.get(0)?,
                model: row.get(1)?,
                pool: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                message_count: row.get(5)?,
            })
        })
        .map_err(|e| format!("读取会话列表失败：{e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("整理会话列表失败：{e}"))
}

fn load_conversation_messages(data_dir: &Path, id: &str) -> Result<Vec<Value>, String> {
    let conn = open_db(data_dir).map_err(|e| format!("打开会话库失败：{e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT message_json FROM conversation_messages
             WHERE conversation_id = ?1 ORDER BY seq ASC, id ASC",
        )
        .map_err(|e| format!("读取会话消息失败：{e}"))?;
    let rows = stmt
        .query_map(params![id], |row| row.get::<_, String>(0))
        .map_err(|e| format!("读取会话消息失败：{e}"))?;
    rows.map(|row| {
        let text = row.map_err(|e| format!("读取会话消息失败：{e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("会话消息格式错误：{e}"))
    })
    .collect()
}

/// 将本机外置 API 会话导出为 JSON + Markdown，便于在切换账号后保留可读存档。
/// 导出的消息是用户主动发送到本地网关的内容；默认不包含任何 JWT/Cookie。
pub fn export_archive(data_dir: &Path) -> Result<ConversationExportResult, String> {
    let summaries = list_summaries(data_dir)?;
    let export_dir = data_dir.join("data").join("exports");
    std::fs::create_dir_all(&export_dir).map_err(|e| format!("创建导出目录失败：{e}"))?;
    let stamp = now_ts();
    let stem = format!("trae_api_conversations_{stamp}");
    let json_path = export_dir.join(format!("{stem}.json"));
    let markdown_path = export_dir.join(format!("{stem}.md"));

    let mut archive = Vec::with_capacity(summaries.len());
    let mut markdown = String::from("# Trae API 本机会话存档\n\n");
    let mut message_count = 0usize;
    for summary in &summaries {
        let messages = load_conversation_messages(data_dir, &summary.conversation_id)?;
        message_count += messages.len();
        archive.push(serde_json::json!({
            "conversation_id": summary.conversation_id,
            "model": summary.model,
            "pool": summary.pool,
            "created_at": summary.created_at,
            "updated_at": summary.updated_at,
            "messages": messages,
        }));
        markdown.push_str(&format!(
            "## {}\n\n- 模型：`{}`\n- 资源池：`{}`\n- 更新时间：{}\n\n",
            summary.conversation_id, summary.model, summary.pool, summary.updated_at
        ));
        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("unknown");
            let content = match message.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(value) => serde_json::to_string_pretty(value).unwrap_or_default(),
                None => String::new(),
            };
            markdown.push_str(&format!("### {}\n\n{}\n\n", role, content));
        }
    }
    let json = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "aiwork-conversation-archive",
        "version": 1,
        "exported_at": stamp,
        "conversations": archive,
    }))
    .map_err(|e| format!("序列化导出文件失败：{e}"))?;
    std::fs::write(&json_path, json).map_err(|e| format!("写入 JSON 导出失败：{e}"))?;
    std::fs::write(&markdown_path, markdown).map_err(|e| format!("写入 Markdown 导出失败：{e}"))?;
    Ok(ConversationExportResult {
        conversations: summaries.len(),
        messages: message_count,
        json_path: json_path.to_string_lossy().into_owned(),
        markdown_path: markdown_path.to_string_lossy().into_owned(),
    })
}

pub fn delete_conversation(data_dir: &Path, id: &str) -> Result<bool, String> {
    let id = id.trim();
    if id.is_empty() || id.chars().count() > MAX_ID_LEN { return Err("conversation_id 为空或过长".into()); }
    let conn = open_db(data_dir).map_err(|e| format!("打开会话库失败：{e}"))?;
    let tx = conn.unchecked_transaction().map_err(|e| format!("删除会话失败：{e}"))?;
    tx.execute("DELETE FROM conversation_messages WHERE conversation_id = ?1", params![id]).map_err(|e| format!("删除会话消息失败：{e}"))?;
    let changed = tx.execute("DELETE FROM conversations WHERE conversation_id = ?1", params![id]).map_err(|e| format!("删除会话索引失败：{e}"))?;
    tx.commit().map_err(|e| format!("提交会话删除失败：{e}"))?;
    Ok(changed > 0)
}

/// 提取并规范化本地会话键。请求体字段优先，随后才读取请求头。
/// 不接受空值和超过 200 字符的键，避免把意外的大字段写进索引。
pub fn request_id(headers: &HeaderMap, body: &Value) -> Option<String> {
    let body_id = body
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let header_id = headers
        .get("x-conversation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let id = body_id.or(header_id)?;
    if id.chars().count() > MAX_ID_LEN {
        return None;
    }
    Some(id.to_string())
}

/// 将请求头中的会话键补进请求体，便于统一参与调度和上游改写。
pub fn ensure_request_id(headers: &HeaderMap, body: &mut Value) -> Option<String> {
    let id = request_id(headers, body)?;
    if body
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("conversation_id".into(), Value::String(id.clone()));
        }
    }
    Some(id)
}

fn load_messages(data_dir: &Path, id: &str) -> Vec<Value> {
    let conn = match open_db(data_dir) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT message_json FROM conversation_messages
         WHERE conversation_id = ?1 ORDER BY seq ASC, id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map(params![id], |row| row.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|row| row.ok().and_then(|s| serde_json::from_str(&s).ok()))
        .collect()
}

fn longest_suffix_prefix(existing: &[Value], incoming: &[Value]) -> usize {
    let max = existing.len().min(incoming.len());
    (0..=max)
        .rev()
        .find(|k| existing[existing.len().saturating_sub(*k)..] == incoming[..*k])
        .unwrap_or(0)
}

/// 在发送上游前限制上下文体积。数据库仍保存完整会话；这里只对单次请求做
/// 保守裁剪，并用本地生成的摘要提示保留被省略消息的角色/首句，避免静默丢上下文。
fn trim_for_upstream(messages: Vec<Value>) -> Vec<Value> {
    let encoded = serde_json::to_vec(&messages).unwrap_or_default();
    if encoded.len() <= MAX_UPSTREAM_CONTEXT_BYTES {
        return messages;
    }
    let mut system = Vec::new();
    let mut tail = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        let size = serde_json::to_vec(message).map(|v| v.len()).unwrap_or(0);
        if used.saturating_add(size) > MAX_UPSTREAM_CONTEXT_BYTES.saturating_sub(2048) {
            break;
        }
        used = used.saturating_add(size);
        tail.push(message.clone());
    }
    tail.reverse();
    for message in &messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            system.push(message.clone());
            if system.len() >= 2 { break; }
        }
    }
    let kept_start = messages.len().saturating_sub(tail.len());
    let omitted = messages[..kept_start]
        .iter()
        .filter_map(|message| {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("unknown");
            let content = message.get("content").and_then(Value::as_str).unwrap_or("").trim();
            if content.is_empty() { None } else { Some(format!("- {role}: {}", content.chars().take(180).collect::<String>())) }
        })
        .take(12)
        .collect::<Vec<_>>();
    // A system message can also occur in the retained tail (for example when a
    // client re-sends a full prompt on every turn). Keep the first system copy
    // only; duplicating it changes prompt precedence on some upstreams.
    let mut result = system.clone();
    if !omitted.is_empty() {
        result.push(serde_json::json!({
            "role": "system",
            "content": format!("本地会话较长，已保留最近消息。较早内容摘要（仅供参考）：\n{}", omitted.join("\n")),
        }));
    }
    result.extend(
        tail.into_iter()
            .filter(|message| !system.iter().any(|existing| existing == message)),
    );
    // The reserved budget above leaves room for the local summary. Keep the
    // invariant even when a provider sends unusually large system messages.
    while serde_json::to_vec(&result).map(|v| v.len()).unwrap_or(0) > MAX_UPSTREAM_CONTEXT_BYTES {
        let Some(index) = result
            .iter()
            .position(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        else {
            break;
        };
        result.remove(index);
    }
    result
}

/// 请求只带最新消息时补齐本机会话历史；请求已带完整历史时按尾首重叠去重。
/// 失败时保持原请求，不让会话库影响主请求链路。
pub fn merge_history(data_dir: &Path, id: &str, body: &mut Value) {
    let incoming = match body.get("messages").and_then(Value::as_array) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => return,
    };
    let history = load_messages(data_dir, id);
    if history.is_empty() {
        return;
    }
    let overlap = longest_suffix_prefix(&history, &incoming);
    let mut merged = history;
    merged.extend(incoming.into_iter().skip(overlap));
    body["messages"] = Value::Array(trim_for_upstream(merged));
}

fn response_assistant(response: &Value) -> Option<Value> {
    if response.get("role").and_then(Value::as_str) == Some("assistant") {
        return Some(response.clone());
    }
    response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .cloned()
}

/// 成功响应后追加本轮新增消息和助手回复。按尾首重叠去重，客户端每轮
/// 重发完整 messages 不会造成数据库无限复制。
pub fn record_turn(
    data_dir: &Path,
    id: &str,
    model: &str,
    pool: &str,
    request_body: &[u8],
    response: Option<&Value>,
) {
    if !load_settings(data_dir).persist_body { return; }
    let body: Value = match serde_json::from_slice(request_body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let incoming = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let assistant = response.and_then(response_assistant);
    if incoming.is_empty() && assistant.is_none() {
        return;
    }
    let existing = load_messages(data_dir, id);
    let overlap = longest_suffix_prefix(&existing, &incoming);
    let mut to_append: Vec<Value> = incoming.into_iter().skip(overlap).collect();
    if let Some(a) = assistant {
        to_append.push(a);
    }
    if to_append.is_empty() {
        return;
    }

    let conn = match open_db(data_dir) {
        Ok(c) => c,
        Err(_) => return,
    };
    let now = now_ts();
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(_) => return,
    };
    let _ = tx.execute(
        "INSERT INTO conversations(conversation_id, model, pool, created_at, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(conversation_id) DO UPDATE SET model=excluded.model,
         pool=excluded.pool, updated_at=excluded.updated_at",
        params![id, model, pool, now],
    );
    let next_seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM conversation_messages WHERE conversation_id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    for (offset, message) in to_append.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if let Ok(encoded) = serde_json::to_string(message) {
            let _ = tx.execute(
                "INSERT INTO conversation_messages(conversation_id, seq, role, message_json, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![id, next_seq + offset as i64, role, encoded, now],
            );
        }
    }
    let _ = tx.commit();
}

/// 为每个本地会话和账号生成独立稳定的上游 ID，避免把账号 A 的云端
/// conversation_id 直接复用给账号 B。
pub fn scoped_upstream_id(local_id: &str, uid: &str, kind: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update(b"|");
    h.update(local_id.as_bytes());
    h.update(b"|");
    h.update(uid.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{:02x}", b)).collect();
    format!("api-{}-{}", kind, hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_id_is_used_and_body_id_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("x-conversation-id", "header-id".parse().unwrap());
        let mut body = json!({"messages": []});
        assert_eq!(ensure_request_id(&headers, &mut body).as_deref(), Some("header-id"));
        assert_eq!(body["conversation_id"], json!("header-id"));
        body["conversation_id"] = json!("body-id");
        assert_eq!(request_id(&headers, &body).as_deref(), Some("body-id"));
    }

    #[test]
    fn merge_and_record_round_trip_deduplicates_full_history() {
        let dir = std::env::temp_dir().join(format!("twa_conversation_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = json!({"conversation_id":"c1","messages":[{"role":"user","content":"hello"}]});
        record_turn(&dir, "c1", "m1", "trae", first.to_string().as_bytes(), Some(&json!({"choices":[{"message":{"role":"assistant","content":"hi"}}]})));
        let mut second = json!({"conversation_id":"c1","messages":[{"role":"user","content":"next"}]});
        merge_history(&dir, "c1", &mut second);
        assert_eq!(second["messages"].as_array().unwrap().len(), 3);
        record_turn(&dir, "c1", "m1", "trae", second.to_string().as_bytes(), Some(&json!({"choices":[{"message":{"role":"assistant","content":"done"}}]})));
        let mut full = json!({"conversation_id":"c1","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"next"},{"role":"assistant","content":"done"}]});
        merge_history(&dir, "c1", &mut full);
        assert_eq!(full["messages"].as_array().unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_ids_are_stable_but_account_specific() {
        assert_eq!(scoped_upstream_id("c", "a", "conversation"), scoped_upstream_id("c", "a", "conversation"));
        assert_ne!(scoped_upstream_id("c", "a", "conversation"), scoped_upstream_id("c", "b", "conversation"));
    }

    #[test]
    fn oversized_history_is_trimmed_with_local_summary() {
        let messages = (0..240)
            .map(|i| json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": "x".repeat(4096)}))
            .collect::<Vec<_>>();
        let trimmed = trim_for_upstream(messages);
        let bytes = serde_json::to_vec(&trimmed).unwrap();
        assert!(bytes.len() <= MAX_UPSTREAM_CONTEXT_BYTES);
        assert!(trimmed.iter().any(|m| m.get("content").and_then(Value::as_str).unwrap_or("").contains("本地会话较长")));
        assert!(trimmed.len() < 240);
    }
}

//! 网关设置独立归属（unified-api-gateway-design §8.1）
//!
//! `data/api_gateway_settings.json`：`port / default_model`——网关设置从
//! app_settings.json 抽离，归属与"公共网关"定位一致。
//!
//! 服务器/容器部署可用环境变量覆盖文件值：`AIWORK_PORT`、`AIWORK_BIND`、
//! `AIWORK_DEFAULT_MODEL`、`AIWORK_CORS_ORIGINS`。参考素材的公网基址由
//! `AIWORK_ASSET_PUBLIC_BASE_URL` 可通过环境变量覆盖，也可在网关设置页显式配置；
//! 不会默认暴露本地文件。
//!
//! 一次性迁移：新文件缺失时从 `conf/app_settings.json` 的旧字段
//! （api_port / api_default_model）抽取并落盘新文件；**旧字段保留不删**
//! （防回滚），但网关启动不再读取。

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettings {
    /// 网关监听端口（默认 7864，与既有约定一致）
    #[serde(default = "default_port")]
    pub port: u16,
    /// 默认模型（请求未指定 model 时使用；统一目录内模型）
    #[serde(default = "default_model")]
    pub default_model: String,
    /// 监听地址；默认仅本机。设置为 0.0.0.0 才允许局域网客户端访问。
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    /// 浏览器客户端允许的来源，逗号分隔；为空时不发送 CORS 允许头。
    #[serde(default)]
    pub cors_origins: String,
    /// 参考素材的短时查看基址；为空时不生成公开素材 URL。Seedance 原生上传不依赖此项。
    /// 例如 `https://api.example.com/v1`，不含查询参数。
    #[serde(default)]
    pub asset_public_base_url: String,
    /// Core bridge mode: off, shadow, or enforce.
    #[serde(default = "default_core_mode")]
    pub core_mode: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_port() -> u16 {
    7864
}

fn default_model() -> String {
    crate::api_server::DEFAULT_MODEL.to_string()
}

fn default_listen_host() -> String {
    "127.0.0.1".into()
}

fn default_core_mode() -> String {
    "off".into()
}

impl Default for GatewaySettings {
    fn default() -> Self {
        GatewaySettings {
            port: default_port(),
            default_model: default_model(),
            listen_host: default_listen_host(),
            cors_origins: String::new(),
            asset_public_base_url: String::new(),
            core_mode: default_core_mode(),
            updated_at: 0,
        }
    }
}

fn settings_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("data").join("api_gateway_settings.json")
}

fn app_settings_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("conf").join("app_settings.json")
}

/// 规范化：default_model 空 → 内置默认
fn normalized(mut s: GatewaySettings) -> GatewaySettings {
    if s.default_model.trim().is_empty() {
        s.default_model = default_model();
    }
    let host = s.listen_host.trim();
    s.listen_host = if host.is_empty() {
        default_listen_host()
    } else {
        host.to_string()
    };
    s.cors_origins = s
        .cors_origins
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    s.asset_public_base_url = s.asset_public_base_url.trim().trim_end_matches('/').to_string();
    s
}

/// 将部署环境覆盖叠加到文件配置上。环境变量只在值合法时生效，错误值不阻塞
/// 启动而是回退到文件/默认值，便于容器编排时逐步补齐配置。
fn apply_env(mut s: GatewaySettings) -> GatewaySettings {
    if let Ok(value) = std::env::var("AIWORK_PORT") {
        if let Ok(port) = value.trim().parse::<u16>() {
            if port > 0 {
                s.port = port;
            }
        }
    }
    if let Ok(value) = std::env::var("AIWORK_BIND") {
        if !value.trim().is_empty() {
            s.listen_host = value.trim().to_string();
        }
    }
    if let Ok(value) = std::env::var("AIWORK_DEFAULT_MODEL") {
        if !value.trim().is_empty() {
            s.default_model = value.trim().to_string();
        }
    }
    if let Ok(value) = std::env::var("AIWORK_CORS_ORIGINS") {
        s.cors_origins = value;
    }
    if let Ok(value) = std::env::var("AIWORK_ASSET_PUBLIC_BASE_URL") {
        s.asset_public_base_url = value;
    }
    normalized(s)
}

/// 读取网关设置；新文件缺失时从 app_settings.json 旧字段一次性迁移
/// （迁移即落盘新文件；旧字段保留不删，防回滚，但不再读取）
pub fn load(data_dir: &Path) -> GatewaySettings {
    let path = settings_path(data_dir);
    if path.exists() {
        let s: GatewaySettings = crate::fs_utils::read_json(&path);
        return apply_env(s);
    }
    // 一次性迁移：旧字段缺失/损坏均回退默认值（read_json 语义一致）
    let legacy: serde_json::Value = crate::fs_utils::read_json(&app_settings_path(data_dir));
    let port = legacy
        .get("api_port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= u64::from(u16::MAX))
        .map(|p| p as u16)
        .unwrap_or_else(default_port);
    let model = legacy
        .get("api_default_model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let s = apply_env(GatewaySettings {
        port,
        default_model: model,
        listen_host: default_listen_host(),
        cors_origins: String::new(),
        asset_public_base_url: String::new(),
        core_mode: default_core_mode(),
        updated_at: 0,
    });
    // 迁移落盘失败不阻塞启动（下次启动重试），内存值仍生效
    let _ = crate::fs_utils::write_json(&path, &s);
    s
}

/// 保存网关设置（端口/模型合法性由调用方校验后传入；这里兜底端口范围）
pub fn save(data_dir: &Path, s: GatewaySettings) -> Result<(), String> {
    if s.port == 0 {
        return Err("端口无效（1-65535）".into());
    }
    let mut s = normalized(s);
    s.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::fs_utils::write_json(&settings_path(data_dir), &s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(app_settings: Option<serde_json::Value>) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "twa_gwset_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        if let Some(v) = app_settings {
            std::fs::write(dir.join("conf").join("app_settings.json"), v.to_string()).unwrap();
        }
        Fixture { dir }
    }

    /// 缺失迁移：app_settings 旧字段 → 新文件；旧文件原样保留（防回滚 §9.6）
    #[test]
    fn t01_migrates_from_app_settings_once() {
        let f = fixture(Some(json!({"api_port": 9000, "api_default_model": "glm-5.3"})));
        let s = load(&f.dir);
        assert_eq!(s.port, 9000);
        assert_eq!(s.default_model, "glm-5.3");
        // 迁移即落盘
        assert!(settings_path(&f.dir).exists());
        // 旧字段保留不删
        let legacy_text =
            std::fs::read_to_string(f.dir.join("conf").join("app_settings.json")).unwrap();
        assert!(legacy_text.contains("9000"));
        // 二次读取走新文件（且改旧文件不再生效）
        let mut legacy: serde_json::Value =
            serde_json::from_str(&legacy_text).unwrap();
        legacy["api_port"] = json!(7777);
        std::fs::write(
            f.dir.join("conf").join("app_settings.json"),
            legacy.to_string(),
        )
        .unwrap();
        assert_eq!(load(&f.dir).port, 9000);
    }

    /// 无旧配置 → 默认值（7864 / deepseek-v4-flash），同样落盘
    #[test]
    fn t02_defaults_when_no_legacy() {
        let f = fixture(None);
        let s = load(&f.dir);
        assert_eq!(s.port, 7864);
        assert_eq!(s.default_model, "deepseek-v4-flash");
        assert!(settings_path(&f.dir).exists());
    }

    /// 新文件存在 → 直接读新文件；save 往返 + updated_at
    #[test]
    fn t03_save_roundtrip() {
        let f = fixture(None);
        let _ = load(&f.dir); // 先迁移落盘
        save(
            &f.dir,
            GatewaySettings {
                port: 8000,
                default_model: "kimi-k3".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: String::new(),
                core_mode: default_core_mode(),
                updated_at: 0,
            },
        )
        .unwrap();
        let s = load(&f.dir);
        assert_eq!(s.port, 8000);
        assert_eq!(s.default_model, "kimi-k3");
        assert!(s.updated_at > 0);
        // 空模型名兜底默认
        save(
            &f.dir,
            GatewaySettings {
                port: 8001,
                default_model: "  ".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: String::new(),
                core_mode: default_core_mode(),
                updated_at: 0,
            },
        )
        .unwrap();
        assert_eq!(load(&f.dir).default_model, "deepseek-v4-flash");
        // 端口 0 拒绝
        assert!(save(
            &f.dir,
            GatewaySettings {
                port: 0,
                default_model: "kimi-k3".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: String::new(),
                core_mode: default_core_mode(),
                updated_at: 0
            },
        )
        .is_err());
    }

    #[test]
    fn t04_asset_public_base_is_trimmed_and_backwards_compatible() {
        let f = fixture(None);
        save(
            &f.dir,
            GatewaySettings {
                port: 8002,
                default_model: "deepseek-v4-flash".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: "  https://example.test/v1///  ".into(),
                core_mode: default_core_mode(),
                updated_at: 0,
            },
        )
        .unwrap();
        assert_eq!(load(&f.dir).asset_public_base_url, "https://example.test/v1");
        // 老版本 JSON 缺少字段时 serde default 应回退空值。
        std::fs::write(
            f.dir.join("data").join("api_gateway_settings.json"),
            r#"{"port":8003,"default_model":"x","listen_host":"127.0.0.1","cors_origins":""}"#,
        )
        .unwrap();
        assert!(load(&f.dir).asset_public_base_url.is_empty());
    }

    #[test]
    fn t05_old_settings_default_core_mode_to_off() {
        let f = fixture(None);
        std::fs::write(
            f.dir.join("data").join("api_gateway_settings.json"),
            r#"{"port":8004,"default_model":"x","listen_host":"127.0.0.1","cors_origins":""}"#,
        )
        .unwrap();

        assert_eq!(load(&f.dir).core_mode, "off");
    }
}

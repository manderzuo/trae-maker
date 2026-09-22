use serde::{Deserialize, Serialize};
use std::{env, fs, path::PathBuf};

const CONFIG_FILE: &str = "router.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterConfig {
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub display_name: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    /// Public URL prefix used in short-lived asset content links.
    /// An explicit STARLINK_ROUTER_PUBLIC_BASE_URL environment variable wins.
    #[serde(default)]
    pub public_base_url: String,
    #[serde(default)]
    pub bridge: Option<BridgeConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub base_url: String,
    pub key_id: String,
    pub key_fingerprint: String,
}

impl RouterConfig {
    pub fn defaults(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            host: "127.0.0.1".to_string(),
            port: Self::default_port(),
            display_name: "星链维度分流系统".to_string(),
            default_model: "deepseek-v4-flash".to_string(),
            public_base_url: String::new(),
            bridge: None,
        }
    }

    pub const fn default_port() -> u16 { 7865 }

    pub fn load(data_dir: impl Into<PathBuf>) -> Result<Self, String> {
        let requested = env::var_os("STARLINK_ROUTER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.into());
        let mut config = Self::defaults(requested);
        let config_path = config.data_dir.join(CONFIG_FILE);
        if config_path.exists() {
            let raw = fs::read_to_string(&config_path)
                .map_err(|e| format!("读取路由器配置失败: {e}"))?;
            let persisted: RouterConfig = serde_json::from_str(&raw)
                .map_err(|e| format!("解析路由器配置失败: {e}"))?;
            config = persisted;
        }
        if let Some(host) = env::var_os("STARLINK_ROUTER_HOST") {
            config.host = host.to_string_lossy().trim().to_string();
        }
        if let Some(port) = env::var_os("STARLINK_ROUTER_PORT") {
            config.port = port.to_string_lossy().parse::<u16>()
                .map_err(|_| "STARLINK_ROUTER_PORT 必须是有效端口".to_string())?;
        }
        if let Some(public_base_url) = env::var_os("STARLINK_ROUTER_PUBLIC_BASE_URL") {
            let public_base_url = public_base_url.to_string_lossy().trim().trim_end_matches('/').to_string();
            if !public_base_url.is_empty() {
                config.public_base_url = public_base_url;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn persist(&self) -> Result<(), String> {
        self.validate()?;
        fs::create_dir_all(&self.data_dir)
            .map_err(|e| format!("创建路由器数据目录失败: {e}"))?;
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("序列化路由器配置失败: {e}"))?;
        fs::write(self.data_dir.join(CONFIG_FILE), raw)
            .map_err(|e| format!("保存路由器配置失败: {e}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() || self.host.chars().any(char::is_whitespace) {
            return Err("路由器监听地址不能为空或包含空格".to_string());
        }
        if self.port == 0 {
            return Err("路由器监听端口必须大于 0".to_string());
        }
        if !self.data_dir.is_absolute() {
            return Err("路由器数据目录必须使用绝对路径".to_string());
        }
        if self.display_name.trim().is_empty() {
            return Err("路由器显示名称不能为空".to_string());
        }
        if self.public_base_url.chars().any(char::is_whitespace) {
            return Err("公网素材地址不能包含空白字符".to_string());
        }
        Ok(())
    }
}

fn default_model() -> String { "deepseek-v4-flash".to_string() }

#[cfg(test)]
mod tests {
    use super::RouterConfig;
    use std::{fs, path::PathBuf};

    #[test]
    fn router_defaults_to_port_7865_and_separate_data_directory() {
        let config = RouterConfig::defaults(PathBuf::from(r"D:\gpt\starlink-dimension-router-data"));
        assert_eq!(config.port, 7865);
        assert_eq!(config.display_name, "星链维度分流系统");
    }

    #[test]
    fn persisted_public_base_url_survives_router_restart() {
        let dir = PathBuf::from(format!(r"D:\gpt\starlink-router-config-test-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("router.json"),
            r#"{
                "data_dir": "D:\\gpt\\starlink-dimension-router-data",
                "host": "127.0.0.1",
                "port": 7865,
                "display_name": "星链维度分流系统",
                "default_model": "deepseek-v4-flash",
                "public_base_url": "https://api.gemstory.cn"
            }"#,
        )
        .unwrap();

        let config = RouterConfig::load(&dir).unwrap();
        assert_eq!(config.public_base_url, "https://api.gemstory.cn");
        let _ = fs::remove_dir_all(dir);
    }
}

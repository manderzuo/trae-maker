//! Installed native client identity shared by login, switching and upstream requests.
use std::path::{Path, PathBuf};
use serde::{Serialize, Deserialize};
use crate::models::Settings;

pub const LEGACY_CLIENT_ID: &str = "en1oxy7wnw8j9n";
pub const TRAE_CLIENT_ID: &str = "ono9krqynydwx5";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeClientProfile {
    pub target_app: String,
    pub exe: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub profiles_name: String,
    pub app_version: String,
    pub base_version: String,
    pub version_code: String,
    pub oauth_client_id: String,
    pub auth_from: String,
}

impl NativeClientProfile {
    pub fn storage_path(&self) -> PathBuf { self.data_dir.join("User/globalStorage/storage.json") }
    pub fn user_agent(&self) -> String {
        format!("VSCode {} ({})", self.base_version, if self.target_app == "Trae" { "TRAE CN" } else { "TRAE SOLO CN" })
    }
}

fn product(exe: &Path) -> serde_json::Value {
    exe.parent().and_then(|dir| std::fs::read_to_string(dir.join("resources/app/product.json")).ok())
        .and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn profile(exe: Option<PathBuf>, target: &str, appdata: &Path) -> NativeClientProfile {
    let value = exe.as_ref().map(|p| product(p)).unwrap_or_default();
    let trae = target == "Trae";
    let data_name = if trae { "Trae CN" } else if exe.as_ref().and_then(|p| p.file_name()).is_some_and(|n| n == "TRAE SOLO.exe") { "TRAE SOLO" } else { "TRAE SOLO CN" };
    NativeClientProfile {
        target_app: target.into(), exe,
        data_dir: appdata.join(data_name),
        profiles_name: if trae { "profiles_trae" } else { "profiles" }.into(),
        app_version: value["appVersion"].as_str().unwrap_or(if trae { "3.4.2" } else { "0.1.65" }).into(),
        base_version: value["version"].as_str().unwrap_or("1.107.1").into(),
        version_code: value["traeVersionCode"].as_str().unwrap_or("20250325").into(),
        oauth_client_id: value["iCubeApp"]["authConfig"][if trae { "TRAE" } else { "SOLO" }]["stable"].as_str().unwrap_or(if trae { TRAE_CLIENT_ID } else { LEGACY_CLIENT_ID }).into(),
        auth_from: if trae { "trae" } else { "solo" }.into(),
    }
}

fn is_unified(exe: &Path) -> bool {
    let p = product(exe);
    let parts: Vec<u32> = p["appVersion"].as_str().unwrap_or_default().split('.').take(2)
        .map(|v| v.parse().unwrap_or(0)).collect();
    parts.first().copied().unwrap_or(0) > 3 || (parts.first() == Some(&3) && parts.get(1).copied().unwrap_or(0) >= 4)
}

fn valid(path: &Option<String>) -> Option<PathBuf> {
    path.as_ref().map(|s| PathBuf::from(s.trim())).filter(|p| p.is_file())
}

fn resolve_at(target: &str, settings: &Settings, local: &Path, appdata: &Path, pf: &Path) -> NativeClientProfile {
    let cn = valid(&settings.trae_cn_path).or_else(|| [local.join("Programs/Trae CN/Trae CN.exe"), pf.join("Trae CN/Trae CN.exe")].into_iter().find(|p| p.is_file()));
    if matches!(target.to_ascii_lowercase().as_str(), "trae" | "trae_cn" | "traecn" | "ide") {
        return profile(cn, "Trae", appdata);
    }
    if let Some(custom) = valid(&settings.trae_path) {
        let kind = if custom.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Trae CN.exe")) { "Trae" } else { "TraeWork" };
        return profile(Some(custom), kind, appdata);
    }
    let migrated = std::fs::read_to_string(appdata.join("Trae CN/migration-job.json")).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["status"] == "completed");
    if cn.as_ref().is_some_and(|p| is_unified(p)) || (migrated && cn.is_some()) {
        return profile(cn, "Trae", appdata);
    }
    let old = ["TRAE SOLO CN/TRAE SOLO CN.exe", "TRAE SOLO/TRAE SOLO.exe", "Trae/Trae.exe"].into_iter()
        .flat_map(|name| [local.join("Programs").join(name), pf.join(name)]).find(|p| p.is_file());
    if old.is_some() { return profile(old, "TraeWork", appdata); }
    if cn.is_some() { return profile(cn, "Trae", appdata); }
    profile(None, "TraeWork", appdata)
}

pub fn resolve_for(target: &str, settings: &Settings) -> NativeClientProfile {
    resolve_at(target, settings, &PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()),
        &PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()), &PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_default()))
}

pub fn resolve_default() -> NativeClientProfile {
    let settings = crate::state::resolve_data_dir().ok().map(|dir| crate::fs_utils::read_json::<Settings>(&dir.join("conf/app_settings.json"))).unwrap_or_default();
    resolve_for("TraeWork", &settings)
}

pub fn canonical_target(target: &str) -> String {
    if matches!(target, "TraeWork" | "Trae") { resolve_for(target, &default_settings()).target_app } else { target.into() }
}
fn default_settings() -> Settings {
    crate::state::resolve_data_dir().ok().map(|dir| crate::fs_utils::read_json(&dir.join("conf/app_settings.json"))).unwrap_or_default()
}

pub fn data_dirs(target: &str) -> Vec<PathBuf> {
    if !matches!(target, "TraeWork" | "Trae") { return vec![]; }
    vec![resolve_for(target, &default_settings()).data_dir]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn installation(root: &std::path::Path, name: &str, version: &str) -> std::path::PathBuf {
        let dir = root.join("Programs").join(name);
        fs::create_dir_all(dir.join("resources/app")).unwrap();
        let exe = dir.join(format!("{name}.exe"));
        fs::write(&exe, "fixture").unwrap();
        fs::write(dir.join("resources/app/product.json"), format!(r#"{{"appVersion":"{version}","version":"1.107.1"}}"#)).unwrap();
        exe
    }
    #[test]
    fn unified_install_routes_legacy_alias_without_reading_old_data() {
        let root = std::env::temp_dir().join(format!("aiwork-native-{}", rand::random::<u64>()));
        let new = installation(&root, "Trae CN", "3.4.2");
        installation(&root, "TRAE SOLO CN", "0.1.65");
        let p = resolve_at("TraeWork", &crate::models::Settings::default(), &root, &root.join("roaming"), &root.join("pf"));
        assert_eq!(p.target_app, "Trae");
        assert_eq!(p.exe, Some(new));
        assert_eq!(p.data_dir, root.join("roaming/Trae CN"));
        assert_eq!(p.profiles_name, "profiles_trae");
        assert_eq!(p.oauth_client_id, "ono9krqynydwx5");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn old_install_and_explicit_choice_keep_original_issuer() {
        let root = std::env::temp_dir().join(format!("aiwork-native-{}", rand::random::<u64>()));
        let old = installation(&root, "TRAE SOLO CN", "0.1.65");
        installation(&root, "Trae CN", "3.3.100");
        let mut settings = crate::models::Settings::default();
        let p = resolve_at("TraeWork", &settings, &root, &root, &root);
        assert_eq!(p.target_app, "TraeWork");
        assert_eq!(p.oauth_client_id, "en1oxy7wnw8j9n");
        installation(&root, "Trae CN", "3.4.2");
        settings.trae_path = Some(old.to_string_lossy().into());
        assert_eq!(resolve_at("TraeWork", &settings, &root, &root, &root).target_app, "TraeWork");
        settings.trae_path = Some(root.join("removed.exe").to_string_lossy().into());
        assert_eq!(resolve_at("TraeWork", &settings, &root, &root, &root).target_app, "Trae");
        fs::remove_dir_all(root).unwrap();
    }
}

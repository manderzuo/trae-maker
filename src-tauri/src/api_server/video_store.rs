//! Seedance 视频产物存储。
//!
//! 任务桥收到上游资源地址后，会把视频以流式方式落到可配置目录。默认目录为
//! `data/videos`，服务器部署时可通过 `AIWORK_VIDEO_DIR` 指向独立磁盘或对象存储
//! 同步目录。下载使用 `.part` 临时文件 + 原子改名，避免客户端读到半个文件。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单个视频最大大小（4 GiB）。超过此值会停止下载并删除临时文件。
pub const MAX_VIDEO_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 运行时视频目录：显式环境变量优先，否则跟随应用数据目录。
pub fn storage_dir(data_dir: &Path) -> PathBuf {
    std::env::var_os("AIWORK_VIDEO_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("data").join("videos"))
}

/// 任务 ID 只允许字母、数字、`-` 和 `_`，阻止路径穿越。
fn safe_task_id(task_id: &str) -> Result<&str, String> {
    let value = task_id.trim();
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err("视频任务 ID 无效".into());
    }
    Ok(value)
}

/// 视频文件路径。扩展名固定为 mp4，不接受客户端传入路径。
pub fn artifact_path(data_dir: &Path, task_id: &str) -> Result<PathBuf, String> {
    let id = safe_task_id(task_id)?;
    Ok(storage_dir(data_dir).join(format!("{id}.mp4")))
}

/// 生成客户端可访问的内容地址。服务器部署时用 `AIWORK_PUBLIC_BASE_URL` 返回
/// 完整 HTTPS 地址；未设置时返回相对路径，适配局域网和本机客户端。
pub fn content_url(task_id: &str) -> Result<String, String> {
    let id = safe_task_id(task_id)?;
    let path = format!("/v1/videos/{id}/content");
    Ok(std::env::var("AIWORK_PUBLIC_BASE_URL")
        .ok()
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
        .map(|base| format!("{base}{path}"))
        .unwrap_or(path))
}

/// 将上游 HTTPS 视频地址流式下载到本地。
///
/// 这里不把视频读入内存；先写 `<task>.mp4.part`，成功后原子替换目标文件。
pub fn download_from_url(
    data_dir: &Path,
    task_id: &str,
    url: &str,
) -> Result<(PathBuf, u64), String> {
    let path = artifact_path(data_dir, task_id)?;
    let dir = storage_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|e| format!("创建视频目录失败: {e}"))?;
    let partial = path.with_extension("mp4.part");

    let trimmed = url.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err("视频资源地址不是 HTTP(S) 地址".into());
    }

    let response = super::streaming_agent()
        .get(trimmed)
        .set("accept", "video/mp4,video/*;q=0.9,*/*;q=0.1")
        .call()
        .map_err(|e| format!("下载视频失败: {e}"))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(format!("下载视频失败：上游 HTTP {status}"));
    }
    if let Some(length) = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
    {
        if length > MAX_VIDEO_BYTES {
            return Err(format!("视频文件超过 {} GiB 限制", MAX_VIDEO_BYTES / (1024 * 1024 * 1024)));
        }
    }

    let result = (|| -> Result<u64, String> {
        let mut reader = response.into_reader();
        let mut writer = File::create(&partial).map_err(|e| format!("创建临时视频文件失败: {e}"))?;
        let mut buf = [0u8; 128 * 1024];
        let mut total = 0u64;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("读取视频流失败: {e}"))?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > MAX_VIDEO_BYTES {
                return Err(format!("视频文件超过 {} GiB 限制", MAX_VIDEO_BYTES / (1024 * 1024 * 1024)));
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| format!("写入视频文件失败: {e}"))?;
        }
        writer
            .sync_all()
            .map_err(|e| format!("刷新视频文件失败: {e}"))?;
        Ok(total)
    })();

    match result {
        Ok(size) => {
            // Windows 上目标文件可能是旧产物，先删除再改名，失败时不会影响旧文件。
            if path.exists() {
                fs::remove_file(&path).map_err(|e| format!("替换旧视频文件失败: {e}"))?;
            }
            fs::rename(&partial, &path).map_err(|e| format!("提交视频文件失败: {e}"))?;
            Ok((path, size))
        }
        Err(error) => {
            let _ = fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// 删除超过保留期的产物。只清理本模块生成的 `.mp4` 和陈旧 `.mp4.part`，
/// 以免误删用户放入的其它文件。
pub fn cleanup(data_dir: &Path, retention_secs: u64) -> usize {
    let dir = storage_dir(data_dir);
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_artifact = path.extension().and_then(|v| v.to_str()) == Some("mp4");
        let is_partial = path
            .file_name()
            .and_then(|v| v.to_str())
            .map(|v| v.ends_with(".mp4.part"))
            .unwrap_or(false);
        if !is_artifact && !is_partial {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(now);
        if retention_secs > 0 && now.saturating_sub(old) >= retention_secs && fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn rejects_path_traversal_and_builds_default_path() {
        let root = std::env::temp_dir().join("aiwork_video_store_test");
        assert!(artifact_path(&root, "video-1").unwrap().ends_with("video-1.mp4"));
        assert!(artifact_path(&root, "..\\secret").is_err());
        assert!(artifact_path(&root, "").is_err());
        assert_eq!(content_url("video-1").unwrap(), "/v1/videos/video-1/content");
    }

    #[test]
    fn cleanup_only_removes_old_mp4() {
        let root = std::env::temp_dir().join(format!("aiwork_video_cleanup_{}", std::process::id()));
        let dir = root.join("data").join("videos");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("new.mp4"), b"x").unwrap();
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        assert_eq!(cleanup(&root, u64::MAX), 0);
        assert!(dir.join("new.mp4").exists());
        assert!(dir.join("keep.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn downloads_stream_to_atomic_mp4_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 512];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: video/mp4\r\nConnection: close\r\n\r\nhello world")
                .unwrap();
        });
        let root = std::env::temp_dir().join(format!("aiwork_video_download_{}", std::process::id()));
        let (path, size) = download_from_url(
            &root,
            "video-test",
            &format!("http://{addr}/video.mp4"),
        )
        .unwrap();
        assert_eq!(size, 11);
        assert_eq!(fs::read(&path).unwrap(), b"hello world");
        assert!(!path.with_extension("mp4.part").exists());
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }
}

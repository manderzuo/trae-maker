#!/usr/bin/env python3
"""Minimal stdio MCP bridge for the AI Work Seedance API.

The bridge deliberately contains no Trae credentials. It reads only the gateway
base URL and API key from environment variables and delegates account selection,
Work credits and video storage to AI Work Assistant.

Environment:
  AIWORK_GATEWAY_BASE_URL  default http://127.0.0.1:7864/v1
  AIWORK_API_KEY           API Key created in AI Work Assistant
  AIWORK_VIDEO_DOWNLOAD_DIR optional local video directory; default is Downloads
  SEEDANCE_POLL_TIMEOUT    default 900 seconds
  SEEDANCE_POLL_INTERVAL   default 3 seconds

MCP stdio messages are newline-delimited JSON-RPC objects. This is intentionally
dependency-free so it can run on another Windows/Linux computer or in a small
container alongside Claude Code/DSH.
"""

from __future__ import annotations

import json
import base64
import os
import re
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit, urlunsplit


def _base_url() -> str:
    value = os.environ.get("AIWORK_GATEWAY_BASE_URL", "http://127.0.0.1:7864/v1").strip()
    if not value:
        raise RuntimeError("AIWORK_GATEWAY_BASE_URL 不能为空")
    parsed = urlsplit(value)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise RuntimeError("AIWORK_GATEWAY_BASE_URL 必须是 http/https 地址")
    path = parsed.path.rstrip("/")
    if path in {"/admin", "/admin/v1"}:
        path = "/v1"
    elif not path or path == "/":
        path = "/v1"
    elif not path.endswith("/v1"):
        path = f"{path}/v1"
    return urlunsplit((parsed.scheme, parsed.netloc, path, "", "")).rstrip("/")


def _health_url() -> str:
    """健康检查位于 API 前缀外：/health，而视频接口位于 /v1 下。"""
    base = _base_url()
    return f"{base[:-3]}/health" if base.endswith("/v1") else f"{base}/health"


def _api_key() -> str:
    return os.environ.get("AIWORK_API_KEY", "").strip()


def _http_json(
    method: str,
    url: str,
    payload: dict[str, Any] | None = None,
    extra_headers: dict[str, str] | None = None,
    *,
    attempts: int = 3,
) -> tuple[int, dict[str, Any]]:
    body = None
    headers = {"Accept": "application/json; charset=utf-8"}
    if payload is not None:
        body = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json; charset=utf-8"
    key = _api_key()
    if key:
        headers["Authorization"] = f"Bearer {key}"
    if extra_headers:
        headers.update(extra_headers)

    last_error: Exception | None = None
    for attempt in range(max(1, attempts)):
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=45) as response:
                raw = response.read(2 * 1024 * 1024)
                if not raw:
                    if attempt + 1 < attempts:
                        time.sleep(min(2 ** attempt, 4))
                        continue
                    return response.status, {}
                try:
                    value = json.loads(raw.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as error:
                    last_error = error
                    if attempt + 1 < attempts:
                        time.sleep(min(2 ** attempt, 4))
                        continue
                    return response.status, {"error": {"message": "网关返回了无效 JSON"}}
                return response.status, value if isinstance(value, dict) else {"data": value}
        except urllib.error.HTTPError as error:
            raw = error.read(2 * 1024 * 1024)
            try:
                value = json.loads(raw.decode("utf-8")) if raw else {}
            except (UnicodeDecodeError, json.JSONDecodeError):
                value = {"error": {"message": raw.decode("utf-8", "replace")[:500]}}
            # 只对可恢复的服务端/限流错误重试，参数错误不重复提交。
            if error.code not in {408, 429} and not 500 <= error.code <= 599:
                return error.code, value if isinstance(value, dict) else {"error": value}
            if attempt + 1 < attempts:
                time.sleep(min(2 ** attempt, 4))
                continue
            return error.code, value if isinstance(value, dict) else {"error": value}
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            last_error = error
            if attempt + 1 < attempts:
                time.sleep(min(2 ** attempt, 4))
                continue
            break
    raise RuntimeError(f"网关请求失败：{last_error}")


def _error_message(value: dict[str, Any]) -> str:
    error = value.get("error")
    if isinstance(error, dict):
        message = error.get("message") or error.get("code")
        if message:
            return str(message)
    return str(value.get("message") or value)[:500]


def _task_from_response(value: dict[str, Any]) -> dict[str, Any]:
    # 兼容真实网关 {task:{...}}、{data:{task:{...}}} 以及旧版扁平响应。
    for candidate in (
        value.get("task"),
        value.get("data", {}).get("task") if isinstance(value.get("data"), dict) else None,
        value.get("data"),
        value,
    ):
        if isinstance(candidate, dict):
            return candidate
    return value


def _task_id_from_response(value: dict[str, Any]) -> str:
    task = _task_from_response(value)
    for candidate in (
        task.get("id"),
        task.get("task_id"),
        value.get("id"),
        value.get("task_id"),
        value.get("data", {}).get("id") if isinstance(value.get("data"), dict) else None,
    ):
        if candidate is not None and str(candidate).strip():
            return str(candidate).strip()
    return ""


def _absolute_content_url(value: str) -> str:
    if not value.startswith("/"):
        return value
    base = _base_url()
    # `/v1` is the API prefix; content_url is relative to the same origin.
    origin = base[:-3] if base.endswith("/v1") else base
    return f"{origin}{value}"


def _download_directory() -> Path:
    configured = os.environ.get("AIWORK_VIDEO_DOWNLOAD_DIR", "").strip()
    if configured:
        return Path(configured).expanduser()
    profile = os.environ.get("USERPROFILE", "").strip()
    home = Path(profile) if profile else Path.home()
    return home / "Downloads"


def _default_download_path(task_id: str) -> Path:
    safe_id = re.sub(r"[^A-Za-z0-9._-]+", "-", str(task_id)).strip("-") or "video"
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return _download_directory() / f"aiwork-seedance-{stamp}-{safe_id}.mp4"


def _download(url: str, target: str) -> int:
    destination = Path(target).expanduser()
    destination.parent.mkdir(parents=True, exist_ok=True)
    temp = destination.with_name(destination.name + ".part")
    request = urllib.request.Request(url, headers={"Accept": "video/mp4,video/*;q=0.9,*/*;q=0.1"})
    total = 0
    try:
        with urllib.request.urlopen(request, timeout=60) as response, temp.open("wb") as out:
            while True:
                chunk = response.read(128 * 1024)
                if not chunk:
                    break
                total += len(chunk)
                if total > 4 * 1024 * 1024 * 1024:
                    raise RuntimeError("视频文件超过 4 GiB 限制")
                out.write(chunk)
        temp.replace(destination)
        return total
    except Exception:
        try:
            temp.unlink()
        except OSError:
            pass
        raise


def _detect_local_asset(path: Path) -> str:
    """识别有限的图片/视频容器，避免把任意本地文件上传到网关。"""
    with path.open("rb") as source:
        header = source.read(32)
    if header.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if header.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if header.startswith((b"GIF87a", b"GIF89a")):
        return "image/gif"
    if header.startswith(b"RIFF") and header[8:12] == b"WEBP":
        return "image/webp"
    if len(header) >= 12 and header[4:8] == b"ftyp":
        return "video/mp4"
    if header.startswith(b"\x1a\x45\xdf\xa3"):
        return "video/webm"
    raise ValueError(f"不支持的素材格式：{path.name}（仅支持 PNG/JPEG/GIF/WebP/MP4/WebM）")


def _upload_local_asset(path_value: str) -> str:
    """读取调用方本地素材并上传到网关，返回网关资产 ID。

    只在工具明确收到 image_paths/video_paths 时执行；不会扫描目录、上传
    其它文件，也不会把本地路径交给 Trae 上游。网关负责再次做魔数校验、大小
    限制和 API Key 所有权隔离。
    """
    path = Path(str(path_value)).expanduser()
    if not path.is_file():
        raise ValueError(f"素材文件不存在：{path}")
    size = path.stat().st_size
    if size <= 0:
        raise ValueError(f"素材文件为空：{path}")
    if size > 32 * 1024 * 1024:
        raise ValueError(f"素材超过 32 MiB 限制：{path}")
    mime_type = _detect_local_asset(path)
    encoded = base64.b64encode(path.read_bytes()).decode("ascii")
    payload: dict[str, Any] = {"filename": path.name, "data_base64": encoded}
    if mime_type:
        payload["mime_type"] = mime_type
    return _upload_asset_payload(payload)


def _upload_asset_payload(payload: dict[str, Any]) -> str:
    status, value = _http_json("POST", f"{_base_url()}/assets", payload)
    if status < 200 or status >= 300:
        raise RuntimeError(f"素材上传失败（HTTP {status}）：{_error_message(value)}")
    asset_id = str(value.get("id", "")).strip()
    if not asset_id:
        raise RuntimeError("网关未返回素材 ID")
    return asset_id


def _upload_inline_asset(encoded: str, filename: str, declared_mime: str | None = None) -> str:
    value = str(encoded).strip()
    if not value:
        raise ValueError("内联素材数据不能为空")
    mime_type = declared_mime.strip() if isinstance(declared_mime, str) and declared_mime.strip() else None
    if value.startswith("data:") and "," in value:
        header = value.split(",", 1)[0]
        mime_type = header[5:].split(";", 1)[0].strip() or None
    payload: dict[str, Any] = {"filename": filename, "data_base64": value}
    if mime_type:
        payload["mime_type"] = mime_type
    return _upload_asset_payload(payload)


def _inline_asset_parts(value: Any, filename: str, index: int) -> tuple[str, str, str | None]:
    """兼容 DSH/Claude Code 常见的拖拽附件 JSON 形状。"""
    if isinstance(value, str):
        return value, f"{Path(filename).stem}-{index}{Path(filename).suffix}", None
    if isinstance(value, dict):
        encoded = next(
            (value.get(key) for key in ("data_base64", "base64", "data", "content")
             if isinstance(value.get(key), str) and value.get(key).strip()),
            None,
        )
        if not encoded:
            raise ValueError("拖拽附件缺少 Base64/data 内容")
        name = value.get("filename") or value.get("name") or filename
        if not isinstance(name, str) or not name.strip():
            name = filename
        mime = value.get("mime_type") or value.get("content_type")
        if mime is not None and not isinstance(mime, str):
            raise ValueError("拖拽附件 mime_type 必须是字符串")
        suffix = Path(name).suffix or Path(filename).suffix
        stem = Path(name).stem or Path(filename).stem
        return encoded, f"{stem[:80]}-{index}{suffix}", mime
    raise ValueError("拖拽附件必须是字符串或对象")


def _asset_ids(
    arguments: dict[str, Any],
    path_key: str,
    id_key: str,
    data_key: str | None = None,
    data_filename: str = "reference.bin",
) -> list[str]:
    ids: list[str] = []
    raw_ids = arguments.get(id_key)
    if raw_ids is not None:
        if not isinstance(raw_ids, list):
            raise ValueError(f"{id_key} 必须是字符串数组")
        for value in raw_ids:
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"{id_key} 只能包含非空字符串")
            ids.append(value.strip())
    raw_paths = arguments.get(path_key)
    if raw_paths is not None:
        if not isinstance(raw_paths, list):
            raise ValueError(f"{path_key} 必须是路径数组")
        for value in raw_paths:
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"{path_key} 只能包含非空路径")
            ids.append(_upload_local_asset(value))
    if data_key:
        raw_data = arguments.get(data_key)
        if raw_data is not None:
            if not isinstance(raw_data, list):
                raise ValueError(f"{data_key} 必须是 Base64 字符串或附件对象数组")
            for index, value in enumerate(raw_data, start=1):
                encoded, filename, mime_type = _inline_asset_parts(value, data_filename, index)
                ids.append(
                    _upload_inline_asset(encoded, filename, mime_type)
                    if mime_type
                    else _upload_inline_asset(encoded, filename)
                )
    if len(ids) > 10:
        raise ValueError(f"{id_key} 最多 10 个素材")
    return ids


def aiwork_health() -> dict[str, Any]:
    """只读健康检查；health 位于 /v1 外，避免把 /v1/health 当成有效路由。"""
    status, value = _http_json("GET", _health_url())
    if status < 200 or status >= 300:
        raise RuntimeError(f"AI Work 健康检查失败（HTTP {status}）：{_error_message(value)}")
    return {"http_status": status, **value}


def _wait_for_task(task_id: str, arguments: dict[str, Any]) -> dict[str, Any]:
    task_id = str(task_id).strip()
    if not task_id:
        raise ValueError("task_id 不能为空")
    timeout = max(10, min(24 * 60 * 60, int(float(os.environ.get("SEEDANCE_POLL_TIMEOUT", "900")))))
    interval = max(1, min(30, int(float(os.environ.get("SEEDANCE_POLL_INTERVAL", "3")))))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        code, detail = _http_json("GET", f"{_base_url()}/videos/{task_id}")
        if code < 200 or code >= 300:
            raise RuntimeError(f"视频任务查询失败（HTTP {code}）：{_error_message(detail)}")
        task = _task_from_response(detail)
        state = str(task.get("status", "")).lower()
        if state in {"completed", "failed", "cancelled", "canceled"}:
            if state != "completed":
                raise RuntimeError(f"Seedance 任务失败：{_error_message(task)}")
            content_url = task.get("content_url") or task.get("video_url") or task.get("resource_uri")
            if content_url:
                content_url = _absolute_content_url(str(content_url))
            target = str(arguments.get("download_path", "")).strip()
            destination = Path(target).expanduser() if target else _default_download_path(task_id)
            if not content_url:
                raise RuntimeError("视频任务已完成，但网关没有返回可下载内容")
            result: dict[str, Any] = {
                "status": state,
                "local_path": str(destination),
                "duration": task.get("video_duration"),
            }
            result["download_bytes"] = _download(content_url, str(destination))
            return result
        # 空响应/尚未产生状态都视为 pending；只在下一次轮询前等待。
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(interval, remaining))
    raise TimeoutError(f"视频任务超过 {timeout} 秒仍未完成")


def aiwork_wait(arguments: dict[str, Any]) -> dict[str, Any]:
    task_id = str(arguments.get("task_id", "")).strip()
    if not task_id:
        raise ValueError("task_id 不能为空")
    return _wait_for_task(task_id, arguments)


def seedance_generate(arguments: dict[str, Any]) -> dict[str, Any]:
    prompt = str(arguments.get("prompt", "")).strip()
    if not prompt:
        raise ValueError("prompt 不能为空")
    payload: dict[str, Any] = {
        "model": str(arguments.get("model", "seedance")).strip() or "seedance",
        "prompt": prompt,
        "duration": arguments.get("duration", 5),
        "resolution": arguments.get("resolution", "720p"),
        "ratio": arguments.get("ratio", "16:9"),
    }
    for name in ("image_urls", "video_urls"):
        if name in arguments and arguments[name] is not None:
            payload[name] = arguments[name]
    image_asset_ids = _asset_ids(arguments, "image_paths", "image_asset_ids", "image_data", "reference.png")
    video_asset_ids = _asset_ids(arguments, "video_paths", "video_asset_ids", "video_data", "reference.mp4")
    if image_asset_ids:
        payload["image_asset_ids"] = image_asset_ids
    if video_asset_ids:
        payload["video_asset_ids"] = video_asset_ids
    # 自动提供幂等键，允许网络超时/空响应重试而不重复创建视频任务。
    request_id = str(arguments.get("idempotency_key", "")).strip() or f"seedance-{uuid.uuid4()}"
    url = f"{_base_url()}/videos/generations"
    extra_headers = {"Idempotency-Key": request_id}
    status, value = _http_json("POST", url, payload, extra_headers)
    if status < 200 or status >= 300:
        raise RuntimeError(f"视频任务创建失败（HTTP {status}）：{_error_message(value)}")
    task_id = _task_id_from_response(value)
    if not task_id:
        raise RuntimeError("网关未返回视频任务 ID")
    return _wait_for_task(task_id, arguments)


def _result(request_id: Any, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def handle(message: dict[str, Any]) -> dict[str, Any] | None:
    method = message.get("method")
    request_id = message.get("id")
    if not isinstance(method, str):
        return _error(request_id, -32600, "无效 JSON-RPC 请求") if request_id is not None else None
    if request_id is None:
        return None  # notification
    if method == "initialize":
        params = message.get("params") or {}
        return _result(request_id, {
            "protocolVersion": str(params.get("protocolVersion", "2024-11-05")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "aiwork-seedance", "version": "1.1.0"},
        })
    if method == "tools/list":
        return _result(request_id, {"tools": [
            {
                "name": "aiwork_health",
                "description": "只读检查 AI Work 网关、账号池和积分状态，不生成视频。",
                "inputSchema": {"type": "object", "properties": {}},
            },
            {
                "name": "aiwork_wait",
                "description": "使用已有 task_id 轮询 Seedance 任务，不创建新任务。",
                "inputSchema": {
                    "type": "object",
                    "required": ["task_id"],
                    "properties": {
                        "task_id": {"type": "string"},
                        "download_path": {"type": "string", "description": "可选：覆盖默认 Downloads 保存位置"},
                    },
                },
            },
            {
                "name": "seedance_generate",
                "description": "通过 AI Work Assistant 的 Trae Work CN Seedance 生成视频，等待完成并自动下载到本机 Downloads。",
                "inputSchema": {
                    "type": "object",
                    "required": ["prompt"],
                    "properties": {
                    "prompt": {"type": "string", "description": "视频描述"},
                    "model": {"type": "string", "default": "seedance"},
                    "duration": {"type": "integer", "minimum": 2, "maximum": 15},
                    "resolution": {"type": "string", "enum": ["480p", "720p", "1080p", "4k"]},
                    "ratio": {"type": "string", "enum": ["16:9", "9:16", "1:1", "4:3", "3:4", "21:9"]},
                    "image_urls": {"type": "array", "items": {"type": "string"}, "description": "可选：Trae 原生 tos-... 资源 URI；普通公网 URL 请改用 image_paths/image_data 上传"},
                    "video_urls": {"type": "array", "items": {"type": "string"}, "description": "可选：Trae 原生 tos-... 资源 URI；普通公网 URL 请改用 video_paths/video_data 上传"},
                    "image_paths": {"type": "array", "items": {"type": "string"}, "description": "可选：调用方本地图片绝对路径；只在本次调用上传"},
                    "video_paths": {"type": "array", "items": {"type": "string"}, "description": "可选：调用方本地参考视频绝对路径；只在本次调用上传"},
                    "image_data": {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "object", "properties": {"data_base64": {"type": "string"}, "data": {"type": "string"}, "filename": {"type": "string"}, "mime_type": {"type": "string"}}, "additionalProperties": True}]}, "description": "可选：拖拽附件的 Base64/data URL，或含 data/name/mime_type 的附件对象；只在本次调用上传"},
                    "video_data": {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "object", "properties": {"data_base64": {"type": "string"}, "data": {"type": "string"}, "filename": {"type": "string"}, "mime_type": {"type": "string"}}, "additionalProperties": True}]}, "description": "可选：参考视频 Base64/data URL 或附件对象数组；只在本次调用上传"},
                    "image_asset_ids": {"type": "array", "items": {"type": "string"}, "description": "可选：此前 /assets 返回的图片 ID"},
                    "video_asset_ids": {"type": "array", "items": {"type": "string"}, "description": "可选：此前 /assets 返回的参考视频 ID"},
                    "idempotency_key": {"type": "string"},
                    "download_path": {"type": "string", "description": "可选：覆盖默认 Downloads 保存位置"},
                },
            },
        }]})
    if method == "tools/call":
        params = message.get("params") or {}
        tool_name = params.get("name")
        if tool_name not in {"aiwork_health", "aiwork_wait", "seedance_generate"}:
            return _error(request_id, -32601, "未知工具")
        try:
            arguments = params.get("arguments") or {}
            if tool_name == "aiwork_health":
                result = aiwork_health()
            elif tool_name == "aiwork_wait":
                result = aiwork_wait(arguments)
            else:
                result = seedance_generate(arguments)
            return _result(request_id, {"content": [{"type": "text", "text": json.dumps(result, ensure_ascii=False)}], "isError": False})
        except Exception as exc:  # tool errors are returned to the harness, not logged with credentials
            return _result(request_id, {"content": [{"type": "text", "text": str(exc)[:800]}], "isError": True})
    if method == "ping":
        return _result(request_id, {})
    return _error(request_id, -32601, f"不支持的方法：{method}")


def main() -> int:
    for raw in sys.stdin.buffer:
        try:
            message = json.loads(raw.decode("utf-8"))
            response = handle(message)
            if response is not None:
                sys.stdout.write(json.dumps(response, ensure_ascii=False, separators=(",", ":")) + "\n")
                sys.stdout.flush()
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            sys.stdout.write(json.dumps(_error(None, -32700, f"无效 JSON：{exc}"), ensure_ascii=False) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

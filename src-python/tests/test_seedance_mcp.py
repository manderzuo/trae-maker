"""Seedance MCP 本地素材输入的无网络单元测试。"""

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import seedance_mcp as mcp


class SeedanceMcpTests(unittest.TestCase):
    def test_local_asset_rejects_unknown_format_before_upload(self):
        with patch.object(mcp, "_upload_asset_payload") as upload:
            with self.assertRaisesRegex(ValueError, "不支持的素材格式"):
                mcp._upload_local_asset(str(Path(__file__)))
        upload.assert_not_called()

    def test_local_asset_accepts_png_magic_header(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root) / "first-frame.png"
            source.write_bytes(b"\x89PNG\r\n\x1a\nfixture")
            with patch.object(mcp, "_upload_asset_payload", return_value="asset-png") as upload:
                self.assertEqual(mcp._upload_local_asset(str(source)), "asset-png")
            payload = upload.call_args.args[0]
            self.assertEqual(payload["mime_type"], "image/png")

    def test_http_json_retries_empty_response(self):
        class Response:
            def __init__(self, raw):
                self.status = 200
                self.raw = raw

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, _limit):
                return self.raw

        with patch.object(mcp.urllib.request, "urlopen", side_effect=[Response(b""), Response(b'{"status":"ok"}')]) as request:
            with patch.object(mcp.time, "sleep"):
                status, value = mcp._http_json("GET", "http://example.invalid/health")
        self.assertEqual((status, value), (200, {"status": "ok"}))
        self.assertEqual(request.call_count, 2)

    def test_task_id_accepts_nested_and_legacy_shapes(self):
        self.assertEqual(mcp._task_id_from_response({"task": {"id": "nested"}}), "nested")
        self.assertEqual(mcp._task_id_from_response({"data": {"task": {"id": "data-nested"}}}), "data-nested")
        self.assertEqual(mcp._task_id_from_response({"task_id": "flat"}), "flat")

    def test_health_uses_origin_path_outside_v1(self):
        with patch.object(mcp, "_base_url", return_value="http://192.168.0.17/v1") as base:
            with patch.object(mcp, "_http_json", return_value=(200, {"status": "ok"})) as request:
                result = mcp.aiwork_health()
        self.assertEqual(result["status"], "ok")
        request.assert_called_once_with("GET", "http://192.168.0.17/health")
        base.assert_called_once()

    def test_tools_list_exposes_health_wait_and_generate(self):
        response = mcp.handle({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        names = {item["name"] for item in response["result"]["tools"]}
        self.assertEqual(names, {"aiwork_health", "aiwork_wait", "seedance_generate"})

    def test_asset_ids_accepts_existing_ids_and_local_paths(self):
        with self.subTest("path is passed to uploader"):
            image = Path(__file__).with_name("test_seedance_mcp.py")
            with patch.object(mcp, "_upload_local_asset", return_value="asset-uploaded") as upload:
                ids = mcp._asset_ids(
                    {"image_asset_ids": ["asset-existing"], "image_paths": [str(image)]},
                    "image_paths",
                    "image_asset_ids",
                )
            self.assertEqual(ids, ["asset-existing", "asset-uploaded"])
            upload.assert_called_once_with(str(image))


    def test_asset_ids_rejects_non_string_and_more_than_ten(self):
        with self.assertRaisesRegex(ValueError, "只能包含非空字符串"):
            mcp._asset_ids({"image_asset_ids": [123]}, "image_paths", "image_asset_ids")
        with self.assertRaisesRegex(ValueError, "最多 10 个素材"):
            mcp._asset_ids({"image_asset_ids": [f"a-{i}" for i in range(11)]}, "image_paths", "image_asset_ids")


    def test_asset_ids_accepts_dragged_data_url(self):
        with patch.object(mcp, "_upload_inline_asset", return_value="asset-inline") as upload:
            ids = mcp._asset_ids(
                {"image_data": ["data:image/png;base64,AAAA"]},
                "image_paths",
                "image_asset_ids",
                "image_data",
                "reference.png",
            )
        self.assertEqual(ids, ["asset-inline"])
        upload.assert_called_once_with("data:image/png;base64,AAAA", "reference-1.png")


    def test_asset_ids_accepts_dragged_attachment_object(self):
        with patch.object(mcp, "_upload_inline_asset", return_value="asset-object") as upload:
            ids = mcp._asset_ids(
                {"image_data": [{"data_base64": "AAAA", "name": "cat.png", "mime_type": "image/png"}]},
                "image_paths",
                "image_asset_ids",
                "image_data",
                "reference.png",
            )
        self.assertEqual(ids, ["asset-object"])
        upload.assert_called_once_with("AAAA", "cat-1.png", "image/png")


    def test_seedance_payload_carries_asset_ids_without_logging_or_network(self):
        calls = []

        def fake_http(method, url, payload=None, extra_headers=None):
            calls.append((method, url, payload, extra_headers))
            if method == "POST":
                return 202, {"task": {"id": "video-test"}}
            return 200, {"task": {"id": "video-test", "status": "completed", "content_url": "https://example.invalid/video.mp4"}}

        with patch.object(mcp, "_http_json", side_effect=fake_http), patch.object(mcp.time, "sleep"):
            result = mcp.seedance_generate({"prompt": "test", "image_asset_ids": ["asset-1"]})
        self.assertEqual(result["task_id"], "video-test")
        self.assertEqual(calls[0][2]["image_asset_ids"], ["asset-1"])
        self.assertEqual(calls[0][2]["duration"], 5)
        self.assertEqual(calls[0][2]["resolution"], "720p")
        self.assertEqual(calls[0][2]["ratio"], "16:9")
        self.assertTrue(calls[0][3]["Idempotency-Key"].startswith("seedance-"))


if __name__ == "__main__":
    unittest.main()

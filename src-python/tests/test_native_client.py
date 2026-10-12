import base64
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


class NativeClientMigrationTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(__file__).resolve().parents[2] / ".superpowers/sdd/2026-10-12-trae-cn-unified-migration"
        scratch.mkdir(parents=True, exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.root = Path(self.tmp.name)
        self.env = patch.dict(os.environ, {"APPDATA": str(self.root / "roaming"), "LOCALAPPDATA": str(self.root),
            "ProgramFiles": str(self.root / "pf"), "AIWORKDATA_DIR": str(self.root / "assistant")}, clear=True)
        self.env.start()

    def tearDown(self):
        if "device_proxy" in sys.modules:
            sys.modules["device_proxy"]._logf.close()
        self.env.stop()
        self.tmp.cleanup()

    def install(self, name, version):
        exe = self.root / "Programs" / name / (name + ".exe")
        exe.parent.mkdir(parents=True, exist_ok=True)
        exe.write_text("fixture")
        product = exe.parent / "resources/app/product.json"
        product.parent.mkdir(parents=True, exist_ok=True)
        product.write_text(json.dumps({"appVersion": version, "version": "1.107.1"}), encoding="utf-8")
        return exe

    def test_unified_headers_and_current_directory_ignore_preserved_old_login(self):
        import native_client
        self.install("TRAE SOLO CN", "0.1.65")
        self.install("Trae CN", "3.4.2")
        p = native_client.resolve()
        self.assertEqual(p["target_app"], "Trae")
        self.assertEqual(Path(p["data_dir"]), self.root / "roaming/Trae CN")
        import auto_checkin
        headers = auto_checkin._build_headers("fixture-token", {"device_id":"fixture", "market_user_id":"m", "session_id":"s"})
        self.assertEqual(headers["app-version"], "3.4.2")
        self.assertIn("TRAE CN", headers["user-agent"])
        self.assertEqual(headers["authorization"], "Cloud-IDE-JWT fixture-token")

    def test_legacy_custom_path_keeps_issuer_and_invalid_path_falls_back(self):
        import native_client
        old = self.install("TRAE SOLO CN", "0.1.65")
        self.install("Trae CN", "3.4.2")
        settings = self.root / "assistant/conf/app_settings.json"
        settings.parent.mkdir(parents=True)
        settings.write_text(json.dumps({"trae_path":str(old)}), encoding="utf-8")
        self.assertEqual(native_client.resolve()["oauth_client_id"], "en1oxy7wnw8j9n")
        settings.write_text(json.dumps({"trae_path":str(self.root / "removed.exe")}), encoding="utf-8")
        self.assertEqual(native_client.resolve()["oauth_client_id"], "ono9krqynydwx5")

    def test_proxy_captures_native_envelope_with_confirmed_request_issuer(self):
        import device_proxy
        device_proxy._accounts_cache = None
        jwt = base64.urlsafe_b64encode(b'{"alg":"RS256","typ":"JWT"}').decode().rstrip("=") + "." + base64.urlsafe_b64encode(json.dumps({"data":{"id":"123"}, "exp":1999999999}).encode()).decode().rstrip("=") + ".fixture"
        with patch.object(device_proxy, "ACCOUNTS_FILE", str(self.root / "assistant/data/checkin_accounts.json")):
            device_proxy.try_capture_refresh_token_from_response("api.trae.cn", "/trae/api/v3/oauth/ExchangeToken",
                json.dumps({"Result":{"UserJwt":jwt,"RefreshToken":"fixture-refresh"}}).encode(),
                json.dumps({"ClientID":"ono9krqynydwx5"}).encode())
            account = device_proxy.load_accounts()["accounts"][0]
            self.assertEqual(account["oauth_client_id"], "ono9krqynydwx5")
            self.assertEqual(account["refresh_token"], "fixture-refresh")

    def test_proxy_rejects_cross_account_response_before_any_write(self):
        import device_proxy
        device_proxy._accounts_cache = None
        path = self.root / "assistant/data/checkin_accounts.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"accounts":[{"UserID":"456", "name":"fixture", "jwt":"old-jwt", "refresh_token":"old-rt"}]}))
        before = path.read_bytes()
        jwt = base64.urlsafe_b64encode(b'{"alg":"RS256","typ":"JWT"}').decode().rstrip("=") + "." + base64.urlsafe_b64encode(json.dumps({"data":{"id":"456"}, "exp":1999999999}).encode()).decode().rstrip("=") + ".fixture"
        with patch.object(device_proxy, "ACCOUNTS_FILE", str(path)):
            device_proxy.try_capture_refresh_token_from_response("api.trae.cn", "/oauth/ExchangeToken",
                json.dumps({"Result":{"UserJwt":jwt,"RefreshToken":"wrong-rt"}}).encode(),
                json.dumps({"UserID":"123", "ClientID":"ono9krqynydwx5"}).encode())
            self.assertEqual(path.read_bytes(), before)

    def test_proxy_older_response_keeps_device_binding_with_credentials(self):
        import device_proxy, native_proof
        device_proxy._accounts_cache = None
        def token(exp):
            return base64.urlsafe_b64encode(b'{"alg":"RS256","typ":"JWT"}').decode().rstrip('=')+'.'+base64.urlsafe_b64encode(json.dumps({'data':{'id':'123'},'exp':exp}).encode()).decode().rstrip('=')+'.fixture'
        path=self.root/'assistant/data/checkin_accounts.json';path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(json.dumps({'accounts':[{'UserID':'123','jwt':token(2000000000),'refresh_token':'latest-rt'}]}))
        before=path.read_bytes()
        with patch.object(device_proxy,'ACCOUNTS_FILE',str(path)), patch.object(native_proof,'captured_device') as binding:
            device_proxy.try_capture_refresh_token_from_response('api.trae.cn','/oauth/ExchangeToken',
                json.dumps({'Result':{'UserJwt':token(1999999999),'RefreshToken':'older-rt'}}).encode(),
                json.dumps({'ClientID':'ono9krqynydwx5','DeviceInfo':{'DevicePublicKey':'older-key'}}).encode())
            self.assertEqual(path.read_bytes(),before)
            binding.assert_not_called()


if __name__ == "__main__":
    unittest.main()

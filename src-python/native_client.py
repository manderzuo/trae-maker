"""Native TRAE identity. Desktop children consume Rust's resolved profile verbatim.

Independent scheduled/scripts detect the same installed product; moving a login
directory never changes an existing refresh token's OAuth issuer.
"""
import json
import os
from pathlib import Path

LEGACY_CLIENT_ID = "en1oxy7wnw8j9n"
TRAE_CLIENT_ID = "ono9krqynydwx5"


def _json(path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        return {}


def resolve():
    try:
        shared = json.loads(os.environ.get("AIWORK_NATIVE_CLIENT", "null"))
        if isinstance(shared, dict) and shared.get("target_app") in ("Trae", "TraeWork"):
            return shared
    except ValueError:
        pass
    appdata = Path(os.environ.get("APPDATA", ""))
    local = Path(os.environ.get("LOCALAPPDATA", ""))
    pf = Path(os.environ.get("ProgramFiles", ""))
    root = Path(os.environ.get("AIWORK_DATA_DIR") or os.environ.get("AIWORKDATA_DIR") or appdata / "AIWorkAssistant")
    settings = _json(root / "conf/app_settings.json")
    def valid(value):
        p = Path(value.strip()) if isinstance(value, str) and value.strip() else None
        return p if p and p.is_file() else None
    cn = valid(settings.get("trae_cn_path")) or next((p for p in (local / "Programs/Trae CN/Trae CN.exe", pf / "Trae CN/Trae CN.exe") if p.is_file()), None)
    custom = valid(settings.get("trae_path"))
    product = _json(cn.parent / "resources/app/product.json") if cn else {}
    version = product.get("appVersion", "0.0")
    try:
        unified = tuple(int(p) for p in version.split(".")[:2]) >= (3, 4)
    except (ValueError, AttributeError):
        unified = False
    migrated = _json(appdata / "Trae CN/migration-job.json").get("status") == "completed"
    if custom:
        exe, trae = custom, custom.name.lower() == "trae cn.exe"
    elif cn and (unified or migrated):
        exe, trae = cn, True
    else:
        old = next((p for name in ("TRAE SOLO CN/TRAE SOLO CN.exe", "TRAE SOLO/TRAE SOLO.exe", "Trae/Trae.exe")
            for p in (local / "Programs" / name, pf / name) if p.is_file()), None)
        exe, trae = (old, False) if old else (cn, bool(cn))
    product = _json(exe.parent / "resources/app/product.json") if exe else {}
    data_name = "Trae CN" if trae else ("TRAE SOLO" if exe and exe.name == "TRAE SOLO.exe" else "TRAE SOLO CN")
    return {"target_app": "Trae" if trae else "TraeWork", "exe": str(exe) if exe else None,
        "data_dir": str(appdata / data_name), "profiles_name":"profiles_trae" if trae else "profiles",
        "app_version": product.get("appVersion", "3.4.2" if trae else "0.1.65"),
        "base_version":product.get("version", "1.107.1"), "version_code":str(product.get("traeVersionCode", "20250325")),
        "oauth_client_id":product.get("iCubeApp", {}).get("authConfig", {}).get("TRAE" if trae else "SOLO", {}).get("stable", TRAE_CLIENT_ID if trae else LEGACY_CLIENT_ID),
        "auth_from":"trae" if trae else "solo"}


def user_agent(profile=None):
    p = profile or resolve()
    return "VSCode {} ({})".format(p["base_version"], "TRAE CN" if p["target_app"] == "Trae" else "TRAE SOLO CN")


def read_auth(data_dir):
    """Return integrity-checked tc auth only in memory; never log its contents."""
    storage = _json(Path(data_dir) / "User/globalStorage/storage.json")
    return decode_tc(storage.get("iCubeAuthInfo://icube.cloudide", ""))


def decode_tc(value):
    import base64
    import hashlib
    import hmac
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
    from cryptography.hazmat.primitives.padding import PKCS7
    try:
        raw = base64.b64decode(value, validate=True)
        if len(raw) < 54 or raw[:3] != b"tc\x05":
            return None
        salt = bytes(a ^ b for a, b in zip(JQ, WQ))
        derived = hashlib.sha512(hashlib.sha512(raw[6:38]).digest() + salt).digest()
        decryptor = Cipher(algorithms.AES(derived[:16]), modes.CBC(derived[16:32])).decryptor()
        padded = decryptor.update(raw[38:]) + decryptor.finalize()
        unpadder = PKCS7(128).unpadder()
        plain = unpadder.update(padded) + unpadder.finalize()
        if len(plain) < 64 or not hmac.compare_digest(plain[:64], hashlib.sha512(plain[64:]).digest()):
            return None
        return json.loads(plain[64:])
    except (KeyError, ValueError, OSError, TypeError):
        return None


JQ = [82,9,106,213,48,54,165,56,191,64,163,158,129,243,215,251,124,227,57,130,155,47,255,135,52,142,67,68,196,222,233,203,84,123,148,50,166,194,35,61,238,76,149,11,66,250,195,78,8,46,161,102,40,217,36,178,118,91,162,73,109,139,209,37]
WQ = [31,221,168,51,136,7,199,49,177,18,16,89,39,128,236,95,96,81,127,169,25,181,74,13,45,229,122,159,147,201,156,239,160,224,59,77,174,42,245,176,200,235,187,60,131,83,153,97,23,43,4,126,186,119,214,38,225,105,20,99,85,33,12,125]

"""Exercise the pinned ccnc-hda1 client and a synthetic 1.x auth snapshot against HARS.

No production configuration is read. Credentials stay in process memory / private
temporary files and are never included in the report or command line.
"""
import argparse
import asyncio
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import uuid

import aiohttp

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("rv1_client", ROOT / "reference/openpilot/client.py")
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)

def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]

async def ready(http, url, process):
    for _ in range(120):
        if process.poll() is not None:
            raise RuntimeError("isolated server exited before readiness")
        try:
            async with http.get(url + "/api/logs") as r:
                if r.status == 200:
                    return
        except (aiohttp.ClientError, OSError):
            pass
        await asyncio.sleep(.1)
    raise RuntimeError("isolated server readiness deadline exceeded")

async def pairing(http, ui, url):
    async with http.post(ui + "/api/settings/devices/pairing", headers={"X-RoadViewer-Request":"1"}) as r:
        assert r.status == 200
        body = await r.json()
    return await client.pair(http, url, body["code"])

def batch(sequence, size):
    return {
        "batch_id": uuid.uuid4().hex,
        "segments": [{
            "route": "2026-11-15--12-00-00",
            "segment": sequence,
            "files": [{"kind": "rlog.zst", "size": size, "sha256": None}],
        }],
    }


async def upload(http, credentials, sequence):
    data = (b"isolated-client-protocol-fixture-" + str(sequence).encode()) * 8192
    digest = hashlib.sha256(data).hexdigest()
    manifest = batch(sequence, len(data))
    session = await client.begin(http, credentials, manifest)
    assert session["checksum_mode"] == "sha256-stream"
    assert 1 <= session["chunk_size"] <= client.MAX_CHUNK_SIZE
    first = data[:65536]
    response = await client.session_request(http, credentials["url"], session, "PUT", "/files/0?offset=0", first)
    assert response["received"] == len(first)
    # A lost chunk response is safe to replay byte-for-byte.
    replay = await client.session_request(http, credentials["url"], session, "PUT", "/files/0?offset=0", first)
    assert replay["received"] == len(first)
    # Reconnect by signed manifest; negotiated token changes and disk offset survives.
    renewed = await client.begin(http, credentials, manifest)
    assert renewed["id"] == session["id"] and renewed["files"][0]["received"] == len(first)
    try:
        await client.session_request(http, credentials["url"], session)
    except client.Error as error:
        assert error.code == "invalid_upload_token"
    else:
        raise AssertionError("rotated token still accepted")
    await client.session_request(http, credentials["url"], renewed, "PUT", "/files/0?offset=65536", data[65536:])
    hashes = json.dumps({"sha256":[digest]}).encode()
    result = await client.session_request(http, credentials["url"], renewed, "POST", "/finish", hashes)
    assert all(isinstance(result.get(key), list) for key in ("logs","duplicates","updated"))
    assert len(result["logs"]) == 1
    repeated = await client.session_request(http, credentials["url"], renewed, "POST", "/finish", hashes)
    assert repeated == result
    completed = await client.begin(http, credentials, manifest)
    assert completed["state"] == "completed" and completed["result"] == result

def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

def legacy_snapshot(snapshot, device_url):
    """Create the 1.x on-disk format with fresh synthetic credentials and one completed receipt."""
    now = time.time()
    master = secrets.token_bytes(32)
    device_id, session_id = uuid.uuid4().hex, uuid.uuid4().hex
    token = secrets.token_urlsafe(32)
    manifest = batch(sequence=0, size=16384)
    canonical = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
    result = {"logs": [uuid.uuid4().hex], "duplicates": [], "updated": []}
    snapshot.mkdir()
    state = {
        "master": master.hex(),
        "devices": {
            device_id: {"device_id": device_id, "name": "Synthetic device", "dongle_id": "",
                        "registered_at": now, "last_seen": now, "revoked": False},
        },
        "nonces": {},
        "receipts": {
            session_id: {"device_id": device_id, "batch_id": manifest["batch_id"],
                         "batch_hash": hashlib.sha256(canonical).hexdigest(),
                         "expires_at": now + 7200, "streaming": True, "chunk_size": client.MAX_CHUNK_SIZE,
                         "token_hash": hashlib.sha256(token.encode()).hexdigest(), "result": result},
        },
    }
    path = snapshot / ".devices.json"
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as file:
        json.dump(state, file)
    credentials = {
        "url": device_url,
        "device_id": device_id,
        "device_secret": hmac.digest(master, f"roadviewer-device-v1:{device_id}".encode(), "sha256").hex(),
    }
    return credentials, {"id": session_id, "token": token}, manifest, result


async def main(args):
    report = {"client_commit": "5c2a8c7f6e14dac82678a390e408ef7a9c73eefa",
              "scope": "Pinned openpilot RV1 client with synthetic uploads and a 1.x auth snapshot"}
    with tempfile.TemporaryDirectory(prefix="hars-compat-") as temp:
        root = Path(temp)
        snapshot, target = root / "offline-snapshot", root / "hars-data"
        rust_ui = f"http://127.0.0.1:{port()}"
        rust_device = f"http://127.0.0.1:{port()}"
        credentials, session, manifest, result = legacy_snapshot(snapshot, rust_device)
        imported = subprocess.run([args.rust, "import-auth", str(snapshot), str(target)],
                                  check=True, capture_output=True, text=True)
        assert json.loads(imported.stdout)["imported_devices"] == 1
        (target / ".processing-settings.json").write_text(json.dumps({"concurrency": 1, "auto_convert": False}))
        env = {**os.environ, "HARS_DATA": str(target), "HARS_WEB": str(ROOT / "web"), "HARS_INGRESS_ONLY": "0",
               "HARS_BIND": rust_ui.removeprefix("http://"), "HARS_DEVICE_BIND": rust_device.removeprefix("http://")}
        rust = subprocess.Popen([args.rust], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            async with aiohttp.ClientSession() as http:
                await ready(http, rust_ui, rust)
                completed = {"state": "completed", "result": result}
                assert await client.session_request(http, rust_device, session) == completed
                assert await client.begin(http, credentials, manifest) == completed
                await upload(http, credentials, 1)
                assert not list((target / ".uploads").iterdir())
                report["existing_device_without_repairing"] = "passed"
                report["legacy_receipt_migration"] = "passed"
                paired = await pairing(http, rust_ui, rust_device)
                await upload(http, paired, 2)
                report["rust_client_lifecycle"] = "passed"
        finally:
            stop(rust)
    output = Path(args.output)
    output.parent.mkdir(parents=True,exist_ok=True)
    output.write_text(json.dumps(report,ensure_ascii=False,indent=2) + "\n")
    print("RV1 lifecycle, offline credential migration without re-pairing, and receipt recovery: PASS")

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--rust",required=True)
    p.add_argument("--output",default="artifacts/ccnc-compat.json")
    asyncio.run(main(p.parse_args()))

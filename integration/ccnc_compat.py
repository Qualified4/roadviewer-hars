"""Exercise the pinned ccnc-hda1 client against isolated Python and Rust processes.

No production configuration is read. Credentials stay in process memory / private
temporary files and are never included in the report or command line.
"""
import argparse
import asyncio
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
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

async def upload(http, credentials, sequence):
    data = (b"isolated-client-protocol-fixture-" + str(sequence).encode()) * 8192
    digest = hashlib.sha256(data).hexdigest()
    manifest = {"batch_id":uuid.uuid4().hex,"segments":[{"route":"2026-11-15--12-00-00","segment":sequence,
                "files":[{"kind":"rlog.zst","size":len(data),"sha256":None}]}]}
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
    return renewed, manifest

def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

async def main(args):
    # Python's real access() recognizes its dedicated loopback listener by port 8098.
    with socket.socket() as check:
        check.bind(("127.0.0.1",8098))
    report = {"client_commit":"5c2a8c7f6e14dac82678a390e408ef7a9c73eefa","python_commit":"71add1a04ed505fc2cb6982052db4a9945d0808f",
              "scope":"pinned real client functions on synthetic payloads; not an openpilot device or valid driving-log parity test"}
    with tempfile.TemporaryDirectory(prefix="hars-compat-") as temp:
        root = Path(temp)
        python_data = root / "python-data"
        python_data.mkdir()
        (python_data / ".processing-settings.json").write_text(json.dumps({"concurrency":1,"auto_convert":False}))
        # English path avoids the documented native loader failure on Korean paths.
        legacy = root / "legacy"
        shutil.copytree(ROOT / "reference/python/roadviewer", legacy)
        ui = "http://127.0.0.1:" + str(port())
        env = {**os.environ,"RV_DATA":str(python_data),"RV_INGRESS_ONLY":"0","RV_DEVICE_HOST_PORT":"18443","RV_ARCHIVE_OPTIONS_JSON":json.dumps({"archive_enabled":False,"archive_path":"","archive_gdrive_fill_tracks":False})}
        python = subprocess.Popen([sys.executable,"-m","gunicorn","--chdir",str(legacy / "app"),"--bind",ui.removeprefix("http://"),"--bind","127.0.0.1:8098","--workers","1","--threads","8","server:app"],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        async with aiohttp.ClientSession() as http:
            try:
                await ready(http,ui,python)
                credentials = await pairing(http,ui,"http://127.0.0.1:8098")
                session,manifest = await upload(http,credentials,0)
                assert not list((python_data / ".uploads").iterdir())
                report["python_client_lifecycle"] = "passed"
            finally:
                stop(python)
            snapshot = root / "offline-snapshot"
            snapshot.mkdir()
            shutil.copy2(python_data / ".devices.json",snapshot / ".devices.json")
            target = root / "hars-data"
            imported = subprocess.run([args.rust,"import-auth",str(snapshot),str(target)],check=True,capture_output=True,text=True)
            assert json.loads(imported.stdout)["imported_devices"] == 1
            rust_ui = "http://127.0.0.1:" + str(port())
            rust_device = "http://127.0.0.1:" + str(port())
            env = {**os.environ,"HARS_DATA":str(target),"HARS_WEB":str(ROOT / "reference/python/roadviewer/app/web"),"HARS_INGRESS_ONLY":"0","HARS_BIND":rust_ui.removeprefix("http://"),"HARS_DEVICE_BIND":rust_device.removeprefix("http://")}
            rust = subprocess.Popen([args.rust],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            try:
                await ready(http,rust_ui,rust)
                credentials["url"] = rust_device
                # Existing Python receipt and token are readable before any new pairing.
                old = await client.session_request(http,rust_device,session)
                assert old["state"] == "completed"
                old_batch = await client.begin(http,credentials,manifest)
                assert old_batch["state"] == "completed"
                await upload(http,credentials,1)
                report["rust_client_lifecycle"] = "passed"
                report["existing_python_device_without_repairing"] = "passed"
                report["python_receipt_migration"] = "passed"
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

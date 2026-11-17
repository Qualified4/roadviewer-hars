"""Real ENOSPC on a small tmpfs: upload admission, chunk precheck, credential writes, then recovery.

Needs root for the tmpfs mount, so it is a local check rather than a CI step. Uses the pinned
RV1 client functions, synthetic payloads and loopback ports only. Run:
  sudo <venv>/bin/python integration/upload_enospc.py --rust target/debug/roadviewer-hars
"""
import argparse
import asyncio
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import uuid

import aiohttp

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("rv1_client", ROOT / "reference/openpilot/client.py")
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)
MIB = 1024 * 1024


def free(port):
    with socket.socket() as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", port))


def fill(mount, keep):
    """Leaves `keep` bytes free (tmpfs accounting is page granular)."""
    filler = mount / "filler"
    with filler.open("ab", buffering=0) as out:
        while (room := shutil.disk_usage(mount).free - keep) > 0:
            try:
                out.write(b"\0" * min(MIB, room))
            except OSError:
                break
    return filler


async def expect(coro, code):
    try:
        await coro
    except client.Error as error:
        assert error.code == code, error.code
        return
    raise AssertionError("expected " + code)


async def main(args):
    assert os.geteuid() == 0, "tmpfs mount requires root"
    ports = (args.port, args.port + 1)
    for p in ports:
        free(p)
    ui, device = (f"http://127.0.0.1:{p}" for p in ports)
    report = {"scope": "tmpfs ENOSPC, pinned client.py functions, synthetic payloads"}
    with tempfile.TemporaryDirectory(prefix="hars-enospc-") as temp:
        mount = Path(temp) / "fs"
        mount.mkdir()
        subprocess.run(["mount", "-t", "tmpfs", "-o", f"size={args.size_mib}m", "tmpfs", str(mount)], check=True)
        server = None
        try:
            data = mount / "data"
            data.mkdir()
            (data / ".processing-settings.json").write_text(json.dumps({"auto_convert": False}))
            env = {**os.environ, "HARS_DATA": str(data), "HARS_WEB": str(ROOT / "web"),
                   "HARS_INGRESS_ONLY": "0", "HARS_BIND": f"127.0.0.1:{ports[0]}",
                   "HARS_DEVICE_BIND": f"127.0.0.1:{ports[1]}", "RUST_LOG": "off"}
            start = lambda: subprocess.Popen([args.rust], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

            async def ready(http, process):
                for _ in range(300):
                    assert process.poll() is None, "server exited"
                    with contextlib.suppress(aiohttp.ClientError, OSError):
                        async with http.get(ui + "/health") as r:
                            if r.status == 200:
                                return
                    await asyncio.sleep(.1)
                raise RuntimeError("readiness deadline")

            log = bytes((i * 7) & 255 for i in range(24 * MIB))
            manifest = {"batch_id": uuid.uuid4().hex, "segments": [{"route": "2026-11-16--10-00-00", "segment": 0,
                        "files": [{"kind": "rlog.zst", "size": len(log), "sha256": None}]}]}
            body = json.dumps({"sha256": [hashlib.sha256(log).hexdigest()]}).encode()
            async with aiohttp.ClientSession() as http:
                server = start()
                await ready(http, server)
                async with http.post(ui + "/api/settings/devices/pairing", headers={"X-RoadViewer-Request": "1"}) as r:
                    code = (await r.json())["code"]
                credentials = await client.pair(http, device, code)
                session = await client.begin(http, credentials, manifest)
                await client.session_request(http, device, session, "PUT", "/files/0?offset=0", log[:8 * MIB])

                # Chunk precheck under real low space releases the session like Python.
                filler = fill(mount, 20 * MIB)
                await expect(client.session_request(http, device, session, "PUT", f"/files/0?offset={8 * MIB}",
                                                    log[8 * MIB:16 * MIB]), "insufficient_disk_space")
                await expect(client.session_request(http, device, session), "session_not_found")
                assert not any((data / ".uploads").iterdir()), "session folder kept after 507"
                report["chunk_low_space"] = "507 insufficient_disk_space, session and reservation released"

                # Admission refuses a new batch while space is short.
                await expect(client.begin(http, credentials, manifest), "insufficient_disk_space")
                report["admission_low_space"] = "507 insufficient_disk_space"

                # No space at all: credential persistence fails closed and the file stays valid.
                devices = data / ".devices.json"
                fill(mount, 0)
                before = devices.read_bytes()
                await expect(client.begin(http, credentials, manifest), "insufficient_disk_space")
                assert devices.read_bytes() == before, "credential state changed on ENOSPC"
                json.loads(devices.read_bytes())
                report["credential_write_enospc"] = "request failed, .devices.json unchanged and parseable"
                server.kill()
                server.wait()

                # After space is freed the restarted server accepts the batch again from scratch.
                filler.unlink()
                server = start()
                await ready(http, server)
                session = await client.begin(http, credentials, manifest)
                assert session["files"][0]["received"] == 0
                for offset in range(0, len(log), 8 * MIB):
                    await client.session_request(http, device, session, "PUT", f"/files/0?offset={offset}", log[offset:offset + 8 * MIB])
                result = await client.session_request(http, device, session, "POST", "/finish", body)
                assert len(result["logs"]) == 1
                record = data / result["logs"][0]["id"] / "rlog.zst"
                assert hashlib.sha256(record.read_bytes()).hexdigest() == hashlib.sha256(log).hexdigest()
                assert not any((data / ".uploads").iterdir())
                temps = [p.name for p in data.rglob("*") if p.name.endswith((".tmp", ".creating", ".removing"))]
                assert not temps, temps
                report["recovery_after_space_freed"] = "new session completed, SHA-256 equal, no leftovers"
        finally:
            if server and server.poll() is None:
                server.kill()
                server.wait()
            subprocess.run(["umount", str(mount)], check=False)
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print("tmpfs ENOSPC upload and credential persistence: PASS")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--rust", required=True)
    p.add_argument("--port", type=int, default=18155)
    p.add_argument("--size-mib", type=int, default=160)
    p.add_argument("--output", default="artifacts/upload-enospc.json")
    asyncio.run(main(p.parse_args()))

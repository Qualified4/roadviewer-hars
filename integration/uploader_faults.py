"""Drive the pinned ccnc-hda1 RV1 client functions through injected upload faults against HARS.

A loopback TCP proxy cuts connections after a byte count or swallows a response, and the
server is SIGKILLed at a debug crash point. Synthetic payloads only; no production data,
configuration or credentials are read, and nothing secret is written to the report.
"""
import argparse
import asyncio
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
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


class Proxy:
    """Forwards loopback TCP. An armed `cut` aborts the connection that carries the next
    `cut` client bytes; an armed `swallow` drops the next response and aborts. Faults are
    armed globally because the client reuses pooled keep-alive connections."""

    def __init__(self, upstream):
        self.upstream = upstream
        self.cut = None
        self.cut_times = 0
        self.swallow = False
        self.cuts = 0

    def arm_cut(self, budget, times):
        # aiohttp silently replays an idempotent request once when a reused connection breaks.
        self.cut, self.cut_times = budget, times

    async def handle(self, reader, writer):
        up_reader, up_writer = await asyncio.open_connection("127.0.0.1", self.upstream)
        budget = None

        async def forward_request():
            nonlocal budget
            while block := await reader.read(65536):
                if budget is None and self.cut_times > 0:
                    budget = self.cut
                    self.cut_times -= 1
                if budget is not None and len(block) >= budget:
                    up_writer.write(block[:budget])
                    await up_writer.drain()
                    raise ConnectionAbortedError
                if budget is not None:
                    budget -= len(block)
                up_writer.write(block)
                await up_writer.drain()

        async def forward_response():
            while block := await up_reader.read(65536):
                if self.swallow:
                    self.swallow = False
                    raise ConnectionAbortedError
                writer.write(block)
                await writer.drain()

        tasks = [asyncio.create_task(forward_request()), asyncio.create_task(forward_response())]
        try:
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            if any(t.done() and not t.cancelled() and isinstance(t.exception(), ConnectionAbortedError) for t in tasks):
                self.cuts += 1
            for task in tasks:
                task.cancel()
            for w in (writer, up_writer):
                with contextlib.suppress(Exception):
                    w.transport.abort()


def free(port):
    with socket.socket() as sock:
        # Like the servers' own listeners: only a live listener counts, not TIME_WAIT leftovers.
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", port))


def start(binary, data, ports, crash=None):
    env = {**os.environ, "HARS_DATA": str(data), "HARS_WEB": str(ROOT / "reference/python/roadviewer/app/web"),
           "HARS_INGRESS_ONLY": "0", "HARS_BIND": f"127.0.0.1:{ports[0]}", "HARS_DEVICE_BIND": f"127.0.0.1:{ports[1]}",
           "RUST_LOG": "off"}
    env.pop("HARS_CRASH_POINT", None)
    if crash:
        env["HARS_CRASH_POINT"], env["HARS_CRASH_MARKER"] = crash
    return subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


async def ready(http, ui, process):
    for _ in range(300):
        if process.poll() is not None:
            raise RuntimeError("server exited before readiness")
        with contextlib.suppress(aiohttp.ClientError, OSError):
            async with http.get(ui + "/health") as r:
                if r.status == 200:
                    return
        await asyncio.sleep(.1)
    raise RuntimeError("server readiness deadline exceeded")


async def expect_error(coro, codes):
    try:
        await coro
    except client.Error as error:
        assert error.code in codes, error.code
        return error.code
    raise AssertionError("request unexpectedly succeeded")


def manifest_for(files, number):
    return {"batch_id": uuid.uuid4().hex, "segments": [{"route": "2026-11-16--09-00-00", "segment": number,
            "files": [{"kind": kind, "size": len(data), "sha256": None} for kind, data in files]}]}


def payloads(seed, size):
    log = bytes((i * 31 ^ seed) & 255 for i in range(size))
    video = bytes((i + seed) & 255 for i in range(size // 2 + 7))
    return [("rlog.zst", log), ("qcamera.ts", video)]


def checksums(files):
    return json.dumps({"sha256": [hashlib.sha256(data).hexdigest() for _, data in files]}).encode()


async def send_file(http, url, session, index, data, start, size):
    offset = start
    while offset < len(data):
        part = data[offset:offset + size]
        result = await client.session_request(http, url, session, "PUT", f"/files/{index}?offset={offset}", part)
        assert result["received"] == offset + len(part)
        offset += len(part)


def invariants(data, files_by_record):
    leftovers = [p.name for p in (data / ".uploads").iterdir()]
    assert not leftovers, leftovers
    temps = [str(p) for p in data.rglob("*") if p.name.endswith((".tmp", ".creating", ".removing"))]
    assert not temps, temps
    records = {}
    for folder in data.iterdir():
        if folder.is_dir() and (folder / "meta.json").is_file():
            meta = json.loads((folder / "meta.json").read_text())
            for kind, info in meta["content_hashes"].items():
                assert hashlib.sha256((folder / kind).read_bytes()).hexdigest() == info["sha256"]
            log = meta["content_hashes"]["rlog.zst"]["sha256"]
            assert log not in records, "log registered twice"
            records[log] = meta["id"]
    expected = {hashlib.sha256(files[0][1]).hexdigest() for files in files_by_record}
    assert set(records) == expected, "registered set differs"
    return records


async def listed(http, ui, data):
    async with http.get(ui + "/api/logs") as r:
        ids = [m["id"] for m in (await r.json())["logs"]]
    assert len(ids) == len(set(ids))
    return set(ids)


async def main(args):
    ports = list(range(args.port, args.port + 3))
    for p in ports:
        free(p)  # Never stop another process: fail if any test port is taken.
    ui, device, proxied = (f"http://127.0.0.1:{p}" for p in ports)
    report = {"client_commit": "5c2a8c7f6e14dac82678a390e408ef7a9c73eefa",
              "scope": "pinned client.py functions, synthetic payloads, loopback proxy faults and SIGKILL"}
    with tempfile.TemporaryDirectory(prefix="hars-uploader-faults-") as temp:
        data = Path(temp) / "data"
        data.mkdir()
        (data / ".processing-settings.json").write_text(json.dumps({"auto_convert": False}))
        marker = Path(temp) / "marker"
        proxy = Proxy(ports[1])
        listener = await asyncio.start_server(proxy.handle, "127.0.0.1", ports[2])
        server = start(args.rust, data, ports)
        registered = []
        async with aiohttp.ClientSession() as http:
            try:
                await ready(http, ui, server)
                async with http.post(ui + "/api/settings/devices/pairing", headers={"X-RoadViewer-Request": "1"}) as r:
                    code = (await r.json())["code"]
                credentials = await client.pair(http, proxied, code)

                # 1. Connection cut in the middle of a chunk body, then status and resume.
                files = payloads(1, 3 * 1024 * 1024)
                manifest = manifest_for(files, 1)
                session = await client.begin(http, credentials, manifest)
                await send_file(http, proxied, session, 0, files[0][1][:1024 * 1024], 0, 512 * 1024)
                proxy.arm_cut(2 * 1024 * 1024 // 3, 3)
                before = proxy.cuts
                error = await expect_error(client.session_request(http, proxied, session, "PUT", "/files/0?offset=1048576",
                                                                  files[0][1][1024 * 1024:2 * 1024 * 1024]), client.TRANSIENT)
                attempts = proxy.cuts - before
                proxy.arm_cut(None, 0)
                status = await client.session_request(http, proxied, session)
                assert status["files"][0]["received"] == 1024 * 1024, status
                report["connection_cut_mid_chunk"] = {"client_error": error, "transient": True, "cut_attempts": attempts,
                                                      "received_after": status["files"][0]["received"]}

                # 2. Token renewal by re-signing the same manifest; the old token is rejected.
                renewed = await client.begin(http, credentials, manifest)
                assert renewed["id"] == session["id"] and renewed["token"] != session["token"]
                assert renewed["files"][0]["received"] == 1024 * 1024
                await expect_error(client.session_request(http, proxied, session), {"invalid_upload_token"})
                await send_file(http, proxied, renewed, 0, files[0][1], 1024 * 1024, 1024 * 1024)
                await send_file(http, proxied, renewed, 1, files[1][1], 0, 1024 * 1024)
                report["token_renewal"] = "old token invalid_upload_token, offsets kept"

                # 3. Finish response lost after registration: the retry returns the receipt.
                proxy.swallow = True
                await expect_error(client.session_request(http, proxied, renewed, "POST", "/finish", checksums(files)),
                                   client.TRANSIENT)
                result = await client.session_request(http, proxied, renewed, "POST", "/finish", checksums(files))
                assert len(result["logs"]) == 1 and result["duplicates"] == []
                again = await client.begin(http, credentials, manifest)
                assert again == {"state": "completed", "result": result}
                registered.append(files)
                invariants(data, registered)
                report["finish_response_lost"] = "retry returned the stored receipt; batch begin reports completed"
            finally:
                server.kill()
                server.wait()

            # 4. SIGKILL at each finish crash point, restart, uploader retries finish.
            outcomes = {}
            for n, point in enumerate(("finish_after_journal", "finish_after_move", "finish_before_receipt",
                                       "finish_after_receipt", "session_removing"), start=2):
                server = start(args.rust, data, ports, (point, str(marker)))
                try:
                    await ready(http, ui, server)
                    files = payloads(n, 512 * 1024)
                    manifest = manifest_for(files, n)
                    session = await client.begin(http, credentials, manifest)
                    for index, (_, payload) in enumerate(files):
                        await send_file(http, proxied, session, index, payload, 0, 256 * 1024)
                    pending = asyncio.create_task(client.session_request(http, proxied, session, "POST", "/finish", checksums(files)))
                    for _ in range(300):
                        if marker.is_file() and marker.read_text() == point:
                            break
                        await asyncio.sleep(.1)
                    else:
                        raise RuntimeError("crash point not reached: " + point)
                    server.kill()
                    server.wait()
                    await expect_error(pending, client.TRANSIENT)
                    marker.unlink()
                finally:
                    if server.poll() is None:
                        server.kill()
                        server.wait()
                server = start(args.rust, data, ports)
                try:
                    await ready(http, ui, server)
                    result = await client.session_request(http, proxied, session, "POST", "/finish", checksums(files))
                    assert len(result["logs"]) == 1, point
                    registered.append(files)
                    records = invariants(data, registered)
                    assert await listed(http, ui, data) == set(records.values())
                    outcomes[point] = "recovered"
                finally:
                    server.kill()
                    server.wait()
            server = start(args.rust, data, ports)
            try:
                await ready(http, ui, server)
                records = invariants(data, registered)
                assert await listed(http, ui, data) == set(records.values())
            finally:
                server.kill()
                server.wait()
            report["sigkill_finish_points"] = outcomes
            report["registered_recordings"] = len(registered)
            report["proxy_cuts"] = proxy.cuts
        listener.close()
        await listener.wait_closed()
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print("RV1 client fault injection (cut, renewal, lost finish, SIGKILL recovery): PASS")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--rust", required=True, help="debug roadviewer-hars executable (crash points are debug-only)")
    p.add_argument("--port", type=int, default=18150, help="first of three consecutive loopback ports")
    p.add_argument("--output", default="artifacts/uploader-faults.json")
    asyncio.run(main(p.parse_args()))

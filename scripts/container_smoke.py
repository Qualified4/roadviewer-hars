"""Boot a disposable image; exercise real TLS and HTTP without production volumes."""
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

def command(*args, **kw):
    return subprocess.check_output(args, text=True, **kw).strip()

image = sys.argv[1]
name = "hars-smoke-" + uuid.uuid4().hex[:10]
with tempfile.TemporaryDirectory(prefix="hars-tls-") as temp:
    root = Path(temp)
    command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-keyout", str(root / "privkey.pem"), "-out", str(root / "fullchain.pem"), stderr=subprocess.DEVNULL)
    os.chmod(root / "privkey.pem", 0o600)
    (root / "options.json").write_text(json.dumps({"max_upload_mb":512,"device_https_enabled":True}))
    (root / "clock.json").write_text(json.dumps([0,.1,.2,.31,.4,.5,.6,.7,.8,.9]))
    context = ssl.create_default_context(cafile=str(root / "fullchain.pem"))
    started = False
    try:
        command("docker", "run", "-d", "--name", name,
                "-e", "HARS_INGRESS_ONLY=0", "-p", "127.0.0.1::8099", "-p", "127.0.0.1::8443",
                "-v", temp + ":/ssl:ro", "-v", str(root / "options.json") + ":/data/options.json:ro", image)
        started = True
        ui = "http://" + command("docker", "port", name, "8099/tcp")
        tls = "https://" + command("docker", "port", name, "8443/tcp")
        def request(url):
            try:
                with urllib.request.urlopen(url, context=context, timeout=3) as r:
                    return r.status, r.read()
            except urllib.error.HTTPError as e:
                return e.code, e.read()
        for _ in range(60):
            try:
                if request(ui + "/health")[0] == 200:
                    break
            except (OSError, TimeoutError):
                pass
            time.sleep(.25)
        else:
            raise RuntimeError("container did not become healthy")
        status, body = request(ui + "/api/logs")
        assert status == 200 and json.loads(body)["logs"] == []
        assert request(ui + "/api/device/test")[0] == 404
        assert request(tls + "/api/device/test")[0] == 200
        assert request(tls + "/api/logs")[0] == 404
        command("docker", "exec", name, "sh", "-c", "test ! -x /usr/bin/python3 && test ! -x /usr/local/bin/python")
        command("docker","exec",name,"ffmpeg","-v","error","-nostdin","-y","-f","lavfi","-i","testsrc2=size=160x96:rate=10","-t","1","-an","-threads","1","-c:v","libx265","-x265-params","pools=1:frame-threads=1:bframes=0:log-level=error","-f","hevc","/tmp/fcamera.hevc",stderr=subprocess.DEVNULL)
        result=command("docker","exec",name,"roadviewer-hars","remux","/tmp/fcamera.hevc","fcamera.hevc","/ssl/clock.json","/tmp/fcamera.mp4")
        assert json.loads(result)["validated"] is True
        print("Container boot, trusted TLS, API isolation, Python-free runtime, and native VFR HEVC: PASS")
    finally:
        if started:
            subprocess.run(["docker", "rm", "-f", name], check=True, stdout=subprocess.DEVNULL)

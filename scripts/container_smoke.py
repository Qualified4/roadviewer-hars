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
    # Deterministic readiness regression: config checks remain immediate, while
    # the actual TLS daemon starts after a short scheduling delay.
    wrapper=root/"bin"
    wrapper.mkdir()
    launcher=wrapper/"nginx"
    launcher.write_text('#!/bin/sh\ncase "$*" in *"daemon off;"*) sleep 0.3;; esac\nexec /usr/sbin/nginx "$@"\n')
    os.chmod(launcher,0o755)
    # The TLS proxy starts only for a host port read from Supervisor, as in the Python app, so
    # the check serves the one `addons/self/info` answer a mapped port produces.
    info = json.dumps({"result": "ok", "data": {"network": {"8443/tcp": 8443}}})
    (root / "supervisor.conf").write_text(
        "events {} http { server { listen 80; location = /addons/self/info "
        "{ default_type application/json; return 200 '" + info + "'; } } }")
    context = ssl.create_default_context(cafile=str(root / "fullchain.pem"))
    started = False
    try:
        command("docker", "network", "create", name)
        command("docker", "run", "-d", "--name", name + "-supervisor", "--network", name, "--network-alias", "supervisor",
                "-v", temp + ":/ssl:ro", "--entrypoint", "/usr/sbin/nginx", image, "-c", "/ssl/supervisor.conf", "-g", "daemon off;")
        command("docker", "run", "-d", "--name", name, "--network", name, "-e", "SUPERVISOR_TOKEN=smoke",
                "-e", "PATH=/ssl/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
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
    except Exception:
        if started:
            try:
                raw=command("docker","logs",name,stderr=subprocess.STDOUT)
                events=[]
                for line in raw.splitlines():
                    try:
                        event=json.loads(line)
                        message=event.get("fields",{}).get("message")
                        if message:
                            events.append(message)
                    except (ValueError,TypeError):
                        pass
                state=json.loads(command("docker","inspect","--format","{{json .State}}",name))
                print(json.dumps({"container_running":state["Running"],"exit_code":state["ExitCode"],"events":events[-12:]}),flush=True)
            except Exception:
                pass
        raise
    finally:
        if started:
            subprocess.run(["docker", "rm", "-f", name], check=True, stdout=subprocess.DEVNULL)
        subprocess.run(["docker", "rm", "-f", name + "-supervisor"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker", "network", "rm", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

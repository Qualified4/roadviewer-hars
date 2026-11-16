"""Boot a disposable image; exercise real TLS and HTTP without production volumes, and convert,
thumbnail and export synthetic camera videos inside an image that has no media tool.

The inputs in scripts/smoke/ are synthetic (lavfi testsrc2, 10 frames at 160x96), made once with a
development FFmpeg:
    ffmpeg -f lavfi -i testsrc2=size=160x96:rate=10 -t 1 -an -threads 1 -c:v libx264 \
        -pix_fmt yuv420p -bf 0 -g 10 -f mpegts qcamera.ts
    ffmpeg -f lavfi -i testsrc2=size=160x96:rate=10 -t 1 -an -threads 1 -c:v libx265 -pix_fmt yuv420p \
        -x265-params pools=1:frame-threads=1:bframes=0:keyint=10:log-level=error -f hevc fcamera.hevc
"""
import json
import os
from pathlib import Path
import shutil
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
    (root / "ts-clock.json").write_text(json.dumps([i/10 for i in range(10)]))
    for fixture in ("qcamera.ts", "fcamera.hevc"):
        shutil.copy(Path(__file__).resolve().parent / "smoke" / fixture, root / fixture)
        os.chmod(root / fixture, 0o644)
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
        # No media tool in the runtime: the conversions below run on HARS alone.
        command("docker", "exec", name, "sh", "-c",
                "! command -v ffmpeg && ! command -v ffprobe && ! ldconfig -p | grep -E 'lib(av(codec|format|util)|swscale)'")
        def hars(*args):
            # stdout is the one JSON result; the log goes to stderr.
            done=subprocess.run(["docker","exec",name,"roadviewer-hars",*args],capture_output=True,text=True,check=True)
            lines=done.stdout.splitlines()
            assert len(lines)==1, done.stdout
            events=[]
            for line in done.stderr.splitlines():
                try:
                    events.append(json.loads(line).get("fields",{}))
                except ValueError:
                    pass
            return json.loads(lines[0]), events
        def remux(source, kind, clock, output):
            result, events = hars("remux", source, kind, clock, output)
            assert result["validated"] is True, result
            engines=[e.get("engine") for e in events if e.get("event")=="remux_engine"]
            assert engines==["pure"] and not any(e.get("event") in ("remux_fallback","remux_failure") for e in events), events
        def signature(path, kind):
            return hars("__inspect", path, kind)[0]["signature"]
        def jpeg(path):
            head=command("docker","exec",name,"sh","-c",f"head -c 2 {path} | od -An -tx1; stat -c %s {path}").split()
            assert head[:2]==["ff","d8"] and int(head[2])>100, head
        cameras = [("/ssl/fcamera.hevc","fcamera.hevc","/ssl/clock.json","/tmp/fcamera.mp4","fcamera.mp4",0),
                   ("/ssl/qcamera.ts","qcamera.ts","/ssl/ts-clock.json","/tmp/camera.mp4","camera.mp4",1.4)]
        for source, kind, clock, mp4, mp4_kind, offset in cameras:
            remux(source, kind, clock, mp4)
            restored = "/tmp/restored." + kind
            result, events = hars("reverse", mp4, kind, restored, str(offset))
            assert result == {"exported": True, "engine": "pure", "reencoded": False}, result
            assert any(e.get("event")=="reverse_engine" and e.get("engine")=="pure" for e in events), events
            # The original, its MP4 and the export hold the same compressed pictures and parameter sets.
            original = signature(source, kind)
            assert signature(mp4, mp4_kind) == original and signature(restored, kind) == original, kind
            hars("__thumbnail", mp4, "/tmp/thumbnail-" + kind + ".jpg")
            jpeg("/tmp/thumbnail-" + kind + ".jpg")
        print("Container boot, trusted TLS, API isolation, Python- and FFmpeg-free runtime, pure-engine VFR HEVC "
              "and H.264 TS conversion, original-format export and thumbnails: PASS")
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

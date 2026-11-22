#!/usr/bin/env python3
"""Runs HARS's browser tests (tests/browser/*.cjs, Node + Playwright, copied from Python Road Viewer 1.3.18 and owned
by HARS since) against a HARS executable and the web files HARS serves, and judges the run (the gate).

Each test runs in its own working folder: roadviewer/app/web holds what HARS served for every HARS-owned web file
(mirror_web.py), roadviewer/config.yaml is HARS's add-on config, and tests/ links to tests/browser (its fixtures and
Python helpers). Tests that start the Python server get hars_as_python.py as RV_PYTHON (pyshim/). TMPDIR points into
the working folder, so the test's own temporary folders and the browser profile go with it. HARS_EXPECTED_VERSION is
the package version in Cargo.toml. Node loads failure_capture.cjs first: a failed test leaves screenshots of its open
pages.

Every test runs under a supervisor process that is a child subreaper: whatever the test started (Node, the browser,
the Python helpers, HARS) stays below it however it was detached, and is killed when the test ends, times out or the
run is interrupted. With --isolate (default) the supervisor runs in a new user+network namespace (unshare -rn) that
has only loopback, and checks that before the test starts, so the browser cannot reach outside hosts (map tiles).

The gate (expectations.json): every browser-groups.json entry runs once and passes. Nothing is run again for
the verdict. A passing test also fails the gate when it leaves
processes behind or when the server adapter saw a recording folder change where expectations.json does not allow a
HARS restart. A failed test is run once more with a Playwright trace for the artifacts only; that run does not change
the verdict.

  tests/browser/run_browser_tests.py --hars-bin target/debug/roadviewer-hars --out /tmp/rv-browser
  tests/browser/run_browser_tests.py --aggregate /tmp/shard1 /tmp/shard2 --out /tmp/rv-browser

Python (not Node) because the server adapter is Python and process supervision needs prctl and /proc."""
import argparse, concurrent.futures, ctypes, datetime, errno, json, os, re, shutil, signal, socket, subprocess, sys
import tempfile, threading, time, tomllib
from pathlib import Path

# Supervisors run from a copy of this file made when the run starts (an edit during a run must not change the
# runs still to come), so they are told where the original lives.
HERE = Path(os.environ.get("HARS_BROWSER_RUNNER_HOME") or Path(__file__).resolve().parent)
REPO = HERE.parents[1]
TESTS = HERE
WEB = REPO / "web"
sys.path.insert(0, str(HERE / "pyshim"))
from hars_shim import listening_owner  # noqa: E402

PR_SET_CHILD_SUBREAPER = 36
PORT_FIRST, PORT_LAST, PORTS_PER_SLOT = 19000, 19199, 40
MIRROR_RANGE = range(19180, 19200)
# Inside the namespace this must fail with "network unreachable"; outside it, it is an ordinary public address.
OUTSIDE = ("1.1.1.1", 443)
EXPECTATION_KEYS = {"about", "entries", "restart_allowed"}
# Fixtures the tests read by these names; a checkout without them fails before any test starts.
FIXTURES = ("fixtures/black.mp4", "fixtures/ui-review-colour.mp4")


def subreaper():
    ctypes.CDLL(None, use_errno=True).prctl(PR_SET_CHILD_SUBREAPER, 1)


def children_map():
    kids = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        kids.setdefault(ppid, []).append(int(entry))
    return kids


def descendants(root):
    kids, found, todo = children_map(), [], [root]
    while todo:
        for pid in kids.get(todo.pop(), []):
            found.append(pid)
            todo.append(pid)
    return found


def command_of(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace").strip()[:160]
    except OSError:
        return "?"


def reap():
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def exited(pid):
    """A process that has ended and only waits to be reaped (or is already gone): not something left running."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().rsplit(")", 1)[1].split()[0] in "ZX"
    except (OSError, IndexError):
        return True


def kill_tree(root=None):
    """SIGKILL everything below `root` (default: this process) until nothing is left; returns what was still
    running when it was called. Children that had already exited are reaped, not reported."""
    root = root or os.getpid()
    first = None
    for _ in range(100):
        reap()
        alive = descendants(root)
        if first is None:
            first = [(pid, command_of(pid)) for pid in alive if not exited(pid)]
        if not alive:
            break
        for pid in alive:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(.05)
        reap()
    reap()
    return first or []


def static_counts(file):
    text = file.read_text() if file.is_file() else ""
    return {"assertions": len(re.findall(r"\bassert(?:\.\w+)?\(", text)),
            "pass_markers": len(re.findall(r"console\.log\(\s*[`'\"]PASS", text))}


def entries():
    manifest = json.loads((TESTS / "browser-groups.json").read_text())
    out = []
    for group, items in manifest.items():
        for item in items:
            file, env = (item, {}) if isinstance(item, str) else (item["file"], item.get("env", {}))
            label = file[:-4] + "".join(f"[{k.removeprefix('RV_').lower()}={v}]" for k, v in env.items())
            out.append({"label": label, "file": file, "env": env, "group": group})
    return out


def hars_version():
    return tomllib.loads((REPO / "Cargo.toml").read_text())["package"]["version"]


def failure_of(log, path):
    """The errors a test printed (Node prints an uncaught or caught-and-logged error as `Name [CODE]: message` at the
    start of a line, then its stack): how many, and the first one's name, message and line in the test file (`path`,
    the end of its file name in the stack, e.g. browser/maps.cjs)."""
    headers = list(re.finditer(r"^([A-Za-z]*Error)(?: \[[A-Z_]+\])?: (.*)$", log, re.M))
    if not headers:
        return {"errors": 0}
    first = headers[0]
    frame = re.search(r"^\s+at .*?/" + re.escape(path) + r":(\d+):\d+", log[first.end():], re.M)
    return {"errors": len(headers), "error": first.group(1), "message": first.group(2).strip(),
            "line": int(frame.group(1)) if frame else None}


def shim_events(log_path):
    """What the server adapter did (hars.log): HARS restarts on a changed recording folder, and changes it saw where
    a restart is not allowed."""
    text = log_path.read_text(errors="replace") if log_path.exists() else ""
    return {"restarts": len(re.findall(r"^\[shim [^]]*\] restart: ", text, re.M)),
            "refused_restarts": len(re.findall(r"^\[shim [^]]*\] restart refused: ", text, re.M))}


# ---------------------------------------------------------------------------------------------------------------- gate
def expectation_problem(expectations):
    """The gate records the manifest size and which entries may restart HARS."""
    unknown = set(expectations) - EXPECTATION_KEYS
    if unknown:
        return f"unknown keys {sorted(unknown)}"
    if type(expectations.get("entries")) is not int:
        return "entries must be the manifest's entry count"
    if not isinstance(expectations.get("restart_allowed", []), list):
        return "restart_allowed must be a list of entries"
    return None


def judge(result, expectations):
    """pass | fail, from what the run recorded: it passed, left no process and needed no HARS restart it may not have."""
    if result["outcome"] != "pass" or result.get("leftover_processes"):
        return "fail"
    shim = result.get("shim") or {}
    allowed = result["label"] in expectations.get("restart_allowed", [])
    if shim.get("refused_restarts") or (shim.get("restarts") and not allowed):
        return "fail"
    return "pass"


def verdict(results, labels, expectations, require_full):
    """The gate over `results` for the entries `labels`. `require_full`: `labels` must be the whole manifest and its
    size the recorded entry count."""
    reasons = []
    manifest = {e["label"] for e in entries()}
    if require_full and (len(manifest) != expectations["entries"] or set(labels) != manifest):
        reasons.append(f"entries: manifest has {len(manifest)}, expectations.json records {expectations['entries']}, "
                       f"this verdict covers {len(set(labels))}")
    unknown = set(expectations.get("restart_allowed", [])) - manifest
    if unknown:
        reasons.append(f"expectations.json names entries not in the manifest: {sorted(unknown)}")
    counts = {"pass": 0, "fail": 0}
    ran = [r["label"] for r in results]
    dup = sorted({x for x in ran if ran.count(x) > 1})
    if dup or set(ran) != set(labels):
        reasons.append(f"entries run {len(ran)}, expected {len(labels)} "
                       f"(missing {sorted(set(labels) - set(ran))[:5]}, duplicated {dup[:5]})")
    for r in results:
        status = judge(r, expectations)
        counts[status] += 1
        if status == "fail":
            reasons.append(f"{r['label']} fail ({r['outcome']}, {r.get('failure')}"
                           + (f", left {r['leftover_processes']}" if r.get("leftover_processes") else "")
                           + (f", adapter {r['shim']}" if (r.get("shim") or {}).get("restarts")
                              or (r.get("shim") or {}).get("refused_restarts") else "") + ")")
    if not results:
        reasons.append("no test ran")
    return {"ok": not reasons, "counts": counts, "reasons": reasons}


def load_expectations():
    expectations = json.loads((HERE / "expectations.json").read_text())
    problem = expectation_problem(expectations)
    if problem:
        sys.exit(f"expectations.json refused: {problem}")
    return expectations


# ---------------------------------------------------------------------------------------------------------------- supervisor
def isolation_check():
    """Run inside the namespace: only loopback exists and an outside address is unreachable. Returns a failure text."""
    links = [l.split(":")[1].strip() for l in
             subprocess.run(["ip", "-o", "link"], capture_output=True, text=True).stdout.splitlines() if ":" in l]
    if links != ["lo"]:
        return f"network interfaces {links}, expected only lo"
    s = socket.socket()
    s.settimeout(3)
    try:
        s.connect(OUTSIDE)
        return f"connected to {OUTSIDE[0]}:{OUTSIDE[1]}"
    except OSError as e:
        if e.errno != errno.ENETUNREACH:
            return f"outside connect failed with {e!r}, expected network unreachable"
    finally:
        s.close()
    return None


def supervise(spec):
    """One test, inside its own (possibly network-isolated) namespace. Writes result.json in the artifact folder."""
    subreaper()
    artifacts, work = Path(spec["artifacts"]), Path(spec["work"])
    result = {"label": spec["label"], "file": spec["file"], "env": spec["env"]}
    state = {"interrupted": False, "child": None}

    def finish(outcome, **extra):
        leftovers = kill_tree()
        temp_left = sorted(p.name for p in (work / "tmp").iterdir()) if (work / "tmp").is_dir() else []
        shutil.rmtree(work, ignore_errors=True)
        result.update(outcome=outcome, leftover_processes=[c for _, c in leftovers],
                      leftover_temp=temp_left, work_removed=not work.exists(),
                      shim=shim_events(artifacts / "hars.log"), **extra)
        (artifacts / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=1))

    def on_term(*_):
        state["interrupted"] = True
        child = state["child"]
        if child and child.poll() is None:
            child.kill()

    signal.signal(signal.SIGTERM, on_term)
    signal.signal(signal.SIGINT, on_term)
    if spec["isolate"]:
        # Not checked here: isolation_check decides, and says what it found.
        subprocess.run(["ip", "link", "set", "lo", "up"], capture_output=True)
        problem = isolation_check()
        result["network"] = problem or "loopback only; outside address unreachable"
        if problem:
            finish("isolation-failed", exit=None, seconds=0)
            return 0
    shutil.copytree(spec["mirror"], work, symlinks=True)
    (work / "tests").mkdir()
    for item in TESTS.iterdir():
        (work / "tests" / item.name).symlink_to(item)
    (work / "tmp").mkdir()
    shots = artifacts / "screenshots"
    env = {k: v for k, v in os.environ.items() if not k.startswith(("RV_", "HARS_"))}
    env.update(spec["env"])
    env.update(PATH=os.pathsep.join([str(Path(spec["node"]).parent), env.get("PATH", "")]),
               NODE_PATH=str(HERE / "node_modules"), TMPDIR=str(work / "tmp"),
               RV_TEST_VIDEO=str(TESTS / "fixtures/black.mp4"), HARS_EXPECTED_VERSION=spec["version"])
    if spec["failure_shots"]:
        env["HARS_FAILURE_SHOTS"] = str(shots)
    if spec.get("trace"):
        env["HARS_TRACE_DIR"] = str(artifacts / "trace")
    env.update(RV_PYTHON=str(HERE / "hars_as_python.py"), HARS_BIN=spec["hars_bin"], HARS_WEB=spec["web"],
               HARS_PORT_RANGE=spec["ports"], HARS_SHIM_LOG=str(artifacts / "hars.log"))
    if spec["restart_allowed"]:
        env["HARS_SHIM_RESTART"] = "1"
    if spec.get("pw_debug"):
        env["DEBUG"] = "pw:api"
    node = [spec["node"]] + (["--require", str(HERE / "failure_capture.cjs")] if spec["failure_shots"] else [])
    started = time.monotonic()
    with open(artifacts / "test.log", "wb") as log:
        child = subprocess.Popen([*node, spec["script"]], cwd=work, env=env, stdout=log,
                                 stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, start_new_session=True)
        state["child"] = child
        try:
            code = child.wait(timeout=spec["timeout"])
            outcome = "interrupted" if state["interrupted"] else "pass" if code == 0 else "fail"
        except subprocess.TimeoutExpired:
            code, outcome = None, "timeout"
    seconds = round(time.monotonic() - started, 1)
    # A test that ends normally stops its own servers; give them a moment before anything still running counts as left.
    deadline = time.monotonic() + 3
    while outcome in ("pass", "fail") and descendants(os.getpid()) and time.monotonic() < deadline:
        time.sleep(.1)
        reap()
    # Screenshots after a failure the test caught itself (it went on and passed) are not kept.
    shots_on_pass = outcome == "pass" and shots.is_dir()
    if shots_on_pass:
        shutil.rmtree(shots)
    finish(outcome, exit=code, seconds=seconds, screenshots_discarded_on_pass=shots_on_pass,
           failure=failure_of((artifacts / "test.log").read_text(errors="replace"), spec["frame"]))
    return 0


# ---------------------------------------------------------------------------------------------------------------- main
def free_ports(count, candidates):
    """`count` loopback ports from `candidates` that nothing listens on now."""
    found = []
    for port in candidates:
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port))
            except OSError:
                continue
        found.append(port)
        if len(found) == count:
            return found
    sys.exit(f"no {count} free loopback ports in {candidates}")


def start_mirror(args, root):
    """Start HARS once on a throwaway data folder with one recording and copy what it serves. Another runner on the
    same machine may use the same port range: the ports are free ones, and the copy starts only once the HARS
    started here is alive and is the process listening on the port."""
    sys.path.insert(0, str(HERE))
    from mirror_web import mirror
    data, out, record = root / "mirror-data", root / "mirror", "f" * 32
    (data / record).mkdir(parents=True)
    (data / record / "meta.json").write_text(json.dumps({"id": record, "status": "unconverted", "name": record,
                                                         "uploaded": 1, "files": {}}))
    ui, device = free_ports(2, MIRROR_RANGE)
    env = dict(os.environ, HARS_DATA=str(data), HARS_WEB=args.web, HARS_INGRESS_ONLY="0",
               HARS_BIND=f"127.0.0.1:{ui}", HARS_DEVICE_BIND=f"127.0.0.1:{device}")
    with open(Path(args.out) / "mirror-hars.log", "wb") as log:
        hars = subprocess.Popen([args.hars_bin], env=env, stdout=log, stderr=log)
    try:
        for _ in range(300):
            if hars.poll() is not None:
                sys.exit("mirror HARS exited, see mirror-hars.log")
            if listening_owner(ui) == hars.pid:
                break
            time.sleep(.1)
        else:
            sys.exit(f"mirror HARS (pid {hars.pid}) is not listening on 127.0.0.1:{ui}")
        count = mirror(f"http://127.0.0.1:{ui}", args.web, record, out)
        if hars.poll() is not None:
            sys.exit("mirror HARS exited while it was copied, see mirror-hars.log")
    finally:
        hars.terminate()
        hars.wait(timeout=10)
    shutil.copy(REPO / "roadviewer_hars/config.yaml", out / "roadviewer/config.yaml")
    shutil.rmtree(data)
    return out, count


def tool_versions(node):
    pw = json.loads((HERE / "node_modules/playwright-core/package.json").read_text())["version"]
    browsers = json.loads((HERE / "node_modules/playwright-core/browsers.json").read_text())["browsers"]
    shell = next(b for b in browsers if b["name"] == "chromium-headless-shell")
    return {"node": subprocess.run([node, "--version"], capture_output=True, text=True).stdout.strip(),
            "playwright": pw, "chromium_headless_shell_revision": shell["revision"],
            "chromium_version": shell.get("browserVersion")}


def write_summary(out, summary, code):
    summary["exit_code"] = code
    (out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=1))
    return code


def print_gate(gate, expectations):
    print("HARS restart allowed in: " + ", ".join(expectations.get("restart_allowed", [])))
    print("gate: " + ("ok" if gate["ok"] else "FAILED") + " " + json.dumps(gate["counts"]))
    for reason in gate["reasons"]:
        print("  " + reason)


def aggregate(args):
    """The gate over shard folders (each with summary.json and exit-code, the shard command's real exit status):
    together they must cover the whole manifest once, each shard's exit status must agree with its own summary and
    with the verdict recomputed here from its results."""
    expectations, reasons, results = load_expectations(), [], []
    for folder in map(Path, args.aggregate):
        try:
            s = json.loads((folder / "summary.json").read_text())
            code = int((folder / "exit-code").read_text().strip())
        except (OSError, ValueError) as e:
            reasons.append(f"{folder}: no summary.json or exit-code ({e})")
            continue
        mine = verdict(s["results"], s["selected"], expectations, False)
        if code != s.get("exit_code") or (code == 0) != mine["ok"] or s["gate"]["ok"] != mine["ok"]:
            reasons.append(f"{folder}: exit status {code}, summary exit_code {s.get('exit_code')}, summary gate "
                           f"{s['gate']['ok']}, recomputed gate {mine['ok']}")
        results += s["results"]
    gate = verdict(results, [e["label"] for e in entries()], expectations, True)
    gate["reasons"] = reasons + gate["reasons"]
    gate["ok"] = not gate["reasons"]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    print_gate(gate, expectations)
    return write_summary(out, {"aggregate": args.aggregate, "gate": gate, "results": results},
                         0 if gate["ok"] else 1)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--supervise", help=argparse.SUPPRESS)
    p.add_argument("--hars-bin")
    p.add_argument("--web", default=str(WEB))
    p.add_argument("--out", help="artifact folder (created)")
    p.add_argument("--node", default=shutil.which("node"))
    p.add_argument("--jobs", type=int, default=1)
    p.add_argument("--timeout", type=int, default=300,
                   help="seconds per test run (the slowest entry, maps main, takes 61-75 s)")
    p.add_argument("--groups", nargs="*", help="browser-groups.json groups to run (default: all)")
    p.add_argument("--only", nargs="*", help="labels or file names (maps.cjs selects its three entries)")
    p.add_argument("--isolate", choices=["on", "off"], default="on",
                   help="on: loopback-only network namespace per test (unshare -rn); off: no network isolation")
    p.add_argument("--pw-debug", action="store_true", help="DEBUG=pw:api in each test log")
    p.add_argument("--no-failure-shots", action="store_true", help="run Node without failure_capture.cjs")
    p.add_argument("--no-collect", action="store_true", help="no traced collection rerun of failed tests")
    p.add_argument("--aggregate", nargs="+", metavar="SHARD", help="judge shard folders together (no test runs)")
    p.add_argument("--isolation-check", action="store_true",
                   help="run inside the namespace (after `ip link set lo up`): fail unless only loopback is reachable")
    args = p.parse_args()
    # Paths become absolute before anything uses them: tests run in their own working folder, and the server
    # adapter, Node's failure capture and the supervisors get these paths there (HARS_SHIM_LOG, HARS_WEB, ...).
    for name in ("out", "hars_bin", "web", "node"):
        if getattr(args, name):
            setattr(args, name, os.path.abspath(getattr(args, name)))
    if args.aggregate:
        args.aggregate = [os.path.abspath(a) for a in args.aggregate]
    if args.supervise:
        sys.exit(supervise(json.loads(args.supervise)))
    if args.isolation_check:
        problem = isolation_check()
        print(problem or "isolated: only lo, outside address unreachable")
        sys.exit(1 if problem else 0)
    if args.aggregate:
        if not args.out:
            p.error("--aggregate needs --out")
        sys.exit(aggregate(args))
    if not (args.hars_bin and args.out and args.node):
        p.error("--hars-bin, --out and node (on PATH or --node) are required")
    if args.jobs < 1 or args.jobs * PORTS_PER_SLOT > MIRROR_RANGE[0] - PORT_FIRST:
        p.error(f"--jobs must be 1..{(MIRROR_RANGE[0] - PORT_FIRST) // PORTS_PER_SLOT}")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    expectations = load_expectations()
    selected = [e for e in entries() if (not args.groups or e["group"] in args.groups)
                and (not args.only or e["label"] in args.only or e["file"] in args.only)]
    version = hars_version()
    summary = {"started": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
               "jobs": args.jobs, "isolate": args.isolate, "groups": args.groups, "only": args.only,
               "selected": [e["label"] for e in selected], "expected_version": version,
               "expectations": expectations}
    args.hars_bin = str(Path(args.hars_bin).resolve())
    if not (Path(args.hars_bin).is_file() and os.access(args.hars_bin, os.X_OK)):
        summary.update(results=[], gate={"ok": False, "counts": {},
                                                      "reasons": [f"HARS executable {args.hars_bin} not found"]})
        print(f"HARS executable {args.hars_bin} not found", file=sys.stderr)
        sys.exit(write_summary(out, summary, 2))
    missing = [f for f in [*FIXTURES, *(e["file"] for e in selected)]
               if not (TESTS / f).is_file()]
    if missing:
        summary.update(results=[], gate={"ok": False, "counts": {},
                                                      "reasons": [f"missing in {TESTS}: {missing}"]})
        print(f"missing in {TESTS}: {missing}", file=sys.stderr)
        sys.exit(write_summary(out, summary, 1))
    args.node = str(Path(args.node).resolve())
    if args.isolate == "on" and subprocess.run(["unshare", "-rn", "true"], capture_output=True).returncode:
        sys.exit("unshare -rn is not permitted here; use --isolate off only where outside requests are blocked otherwise")
    subreaper()
    root = Path(tempfile.mkdtemp(prefix="hars-browser-", dir=out))
    stop = threading.Event()
    running = {}
    lock = threading.Lock()

    def interrupt(*_):
        stop.set()
        with lock:
            for proc in running.values():
                if proc.poll() is None:
                    proc.send_signal(signal.SIGTERM)

    signal.signal(signal.SIGINT, interrupt)
    signal.signal(signal.SIGTERM, interrupt)
    began = time.monotonic()
    free_slots = list(range(args.jobs))
    results, collected, mirrored = [], [], 0

    def run(entry, collect=False):
        if stop.is_set():
            return None
        with lock:
            slot = free_slots.pop()
        label, script = entry["label"], TESTS / entry["file"]
        try:
            first = PORT_FIRST + slot * PORTS_PER_SLOT
            artifacts = out / label / ("collect" if collect else "")
            artifacts.mkdir(parents=True, exist_ok=True)
            work = root / f"work-{slot}"
            spec = {"label": label, "file": entry["file"], "env": entry["env"], "artifacts": str(artifacts),
                    "script": str(script), "frame": f"{script.parent.name}/{script.name}", "work": str(work),
                    "mirror": str(mirror_root), "node": args.node, "version": version,
                    "hars_bin": args.hars_bin, "web": args.web, "ports": f"{first}-{first + PORTS_PER_SLOT - 1}",
                    "timeout": args.timeout, "isolate": args.isolate == "on", "pw_debug": args.pw_debug,
                    "failure_shots": not args.no_failure_shots,
                    "trace": collect, "restart_allowed": label in expectations.get("restart_allowed", [])}
            command = [sys.executable, str(runner), "--supervise", json.dumps(spec)]
            if spec["isolate"]:
                command = ["unshare", "-rn", "--", *command]
            with lock:
                if stop.is_set():
                    return None
                proc = subprocess.Popen(command, start_new_session=True, stdin=subprocess.DEVNULL)
                running[label, collect] = proc
            try:
                proc.wait(timeout=args.timeout + 60)
            except subprocess.TimeoutExpired:
                # The supervisor is stuck: end what it started (the test runs in a session of its own) and its
                # working folder now, not when the whole run ends.
                kill_tree(proc.pid)
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
                shutil.rmtree(work, ignore_errors=True)
            with lock:
                running.pop((label, collect), None)
            try:
                result = json.loads((artifacts / "result.json").read_text())
            except (OSError, ValueError):
                result = {"label": label, "file": entry["file"], "env": entry["env"],
                          "outcome": "supervisor-failed", "supervisor_exit": proc.returncode}
            result.update(group=entry["group"], **static_counts(script))
            if collect:
                result["collection_rerun"] = True
            tail = (artifacts / "test.log").read_text(errors="replace")[-600:] if (artifacts / "test.log").exists() else ""
            shown = "collect" if collect else result["outcome"]
            print(f"{shown:>8} {result.get('seconds', '-'):>6}s  {label}"
                  + ("" if result["outcome"] == "pass" or collect else "\n    " + tail.strip().replace("\n", "\n    ")[-400:]),
                  flush=True)
            return result
        finally:
            with lock:
                free_slots.append(slot)

    leftovers = []
    try:
        runner = root / "runner.py"
        shutil.copy(Path(__file__).resolve(), runner)
        os.environ["HARS_BROWSER_RUNNER_HOME"] = str(HERE)
        mirror_root, mirrored = start_mirror(args, root)
        with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
            results = [r for r in pool.map(run, selected) if r]
        # Judged first; the traced reruns only gather artifacts for what already failed.
        failed = [r for r in results if judge(r, expectations) != "pass"]
        if not args.no_collect and not stop.is_set():
            by_label = {e["label"]: e for e in selected}
            for r in failed:
                again = run(by_label[r["label"]], collect=True)
                if again:
                    collected.append(again)
    finally:
        leftovers = kill_tree()
        shutil.rmtree(root, ignore_errors=True)
    for r in results:
        r["gate"] = judge(r, expectations)
    gate = verdict(results, [e["label"] for e in selected], expectations, False)
    if stop.is_set():
        gate["ok"] = False
        gate["reasons"].append("interrupted")
    if leftovers:
        gate["ok"] = False
        gate["reasons"].append(f"runner left processes: {[c for _, c in leftovers]}")
    summary.update(seconds=round(time.monotonic() - began, 1), hars_bin=args.hars_bin, mirrored_files=mirrored,
                   interrupted=stop.is_set(), runner_leftover_processes=[c for _, c in leftovers],
                   temp_root_removed=not root.exists(), **tool_versions(args.node),
                   totals={k: sum(1 for r in results if r["outcome"] == k) for k in sorted({r["outcome"] for r in results})},
                   gate=gate, results=results, collection_reruns=collected)
    print_gate(gate, expectations)
    print(json.dumps({k: summary[k] for k in ("seconds", "totals", "interrupted", "runner_leftover_processes",
                                              "temp_root_removed")}, ensure_ascii=False))
    for r in collected:
        print(f"collection rerun (artifacts only, not judged): {r['label']}/collect {r['outcome']}")
    return write_summary(out, summary, 130 if stop.is_set() else 0 if gate["ok"] else 1)


if __name__ == "__main__":
    sys.exit(main())

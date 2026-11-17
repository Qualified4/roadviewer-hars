#!/usr/bin/env python3
"""Runs the preserved Python Road Viewer browser tests (reference/python/tests/*.cjs, Node + Playwright) against a HARS
executable and the web files HARS serves, without changing the test files.

Each test runs in its own working folder: roadviewer/app/web holds what HARS served for every preserved web file
(mirror_web.py), roadviewer/config.yaml is HARS's add-on config, and tests/ links to the preserved tests folder (its
fixtures and Python helpers). Tests that start the Python server get hars_as_python.py as RV_PYTHON (pyshim/). TMPDIR
points into the working folder, so the test's own temporary folders and the browser profile go with it.

Every test runs under a supervisor process that is a child subreaper: whatever the test started (Node, the browser,
the Python helpers, HARS) stays below it however it was detached, and is killed when the test ends, times out or the
run is interrupted. With --isolate (default) the supervisor runs in a new user+network namespace (unshare -rn) that
has only loopback, so the browser cannot reach outside hosts (map tiles).

  integration/upstream_cjs/run_browser_tests.py --hars-bin target/release/roadviewer-hars --out /tmp/rv-browser

Python (not Node) because the server adapter is Python and process supervision needs prctl and /proc."""
import argparse, concurrent.futures, ctypes, datetime, json, os, re, shutil, signal, subprocess, sys, tempfile, threading, time
from pathlib import Path

# Supervisors run from a copy of this file made when the run starts (an edit during a run must not change the
# runs still to come), so they are told where the original lives.
HERE = Path(os.environ.get("HARS_BROWSER_RUNNER_HOME") or Path(__file__).resolve().parent)
REPO = HERE.parents[1]
TESTS = REPO / "reference/python/tests"
WEB = REPO / "reference/python/roadviewer/app/web"
PR_SET_CHILD_SUBREAPER = 36
PORT_FIRST, PORT_LAST, PORTS_PER_SLOT = 19000, 19199, 40
MIRROR_PORTS = (19180, 19181)


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


def kill_tree(root=None):
    """SIGKILL everything below `root` (default: this process) until nothing is left; returns what was found first."""
    root = root or os.getpid()
    first = None
    for _ in range(100):
        alive = descendants(root)
        if first is None:
            first = [(pid, command_of(pid)) for pid in alive]
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
    text = file.read_text()
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


# ---------------------------------------------------------------------------------------------------------------- supervisor
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
                      leftover_temp=temp_left, work_removed=not work.exists(), **extra)
        (artifacts / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=1))

    def on_term(*_):
        state["interrupted"] = True
        child = state["child"]
        if child and child.poll() is None:
            child.kill()

    signal.signal(signal.SIGTERM, on_term)
    signal.signal(signal.SIGINT, on_term)
    if spec["isolate"]:
        subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
        result["network"] = subprocess.run(["ip", "-o", "link"], capture_output=True, text=True).stdout.split("\n")[0][:80]
    shutil.copytree(spec["mirror"], work, symlinks=True)
    (work / "tests").mkdir()
    for item in TESTS.iterdir():
        (work / "tests" / item.name).symlink_to(item)
    (work / "tmp").mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("RV_", "HARS_"))}
    env.update(spec["env"])
    env.update(PATH=os.pathsep.join([str(Path(spec["node"]).parent), env.get("PATH", "")]),
               NODE_PATH=str(HERE / "node_modules"), TMPDIR=str(work / "tmp"),
               RV_TEST_VIDEO=str(TESTS / "fixtures/black.mp4"))
    if spec.get("python_baseline"):
        env.update(RV_PYTHON=spec["python_baseline"], PYTHONDONTWRITEBYTECODE="1")
    else:
        env.update(RV_PYTHON=str(HERE / "hars_as_python.py"), HARS_BIN=spec["hars_bin"], HARS_WEB=spec["web"],
                   HARS_PORT_RANGE=spec["ports"], HARS_SHIM_LOG=str(artifacts / "hars.log"))
    if spec.get("pw_debug"):
        env["DEBUG"] = "pw:api"
    started = time.monotonic()
    with open(artifacts / "test.log", "wb") as log:
        child = subprocess.Popen([spec["node"], str(TESTS / spec["file"])], cwd=work, env=env, stdout=log,
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
    finish(outcome, exit=code, seconds=seconds)
    return 0


# ---------------------------------------------------------------------------------------------------------------- main
def start_mirror(args, root):
    """Start HARS once on a throwaway data folder with one recording and copy what it serves."""
    sys.path.insert(0, str(HERE))
    from mirror_web import mirror
    data, out, record = root / "mirror-data", root / "mirror", "f" * 32
    (data / record).mkdir(parents=True)
    (data / record / "meta.json").write_text(json.dumps({"id": record, "status": "unconverted", "name": record,
                                                         "uploaded": 1, "files": {}}))
    ui, device = MIRROR_PORTS
    env = dict(os.environ, HARS_DATA=str(data), HARS_WEB=args.web, HARS_INGRESS_ONLY="0",
               HARS_BIND=f"127.0.0.1:{ui}", HARS_DEVICE_BIND=f"127.0.0.1:{device}")
    with open(Path(args.out) / "mirror-hars.log", "wb") as log:
        hars = subprocess.Popen([args.hars_bin], env=env, stdout=log, stderr=log)
    try:
        import socket
        for _ in range(300):
            try:
                socket.create_connection(("127.0.0.1", ui), timeout=.2).close()
                break
            except OSError:
                if hars.poll() is not None:
                    sys.exit("mirror HARS exited, see mirror-hars.log")
                time.sleep(.1)
        count = mirror(f"http://127.0.0.1:{ui}", args.web, record, out)
    finally:
        hars.terminate()
        hars.wait(timeout=10)
    shutil.copy(REPO / "roadviewer_hars/config.yaml", out / "roadviewer/config.yaml")
    shutil.rmtree(data)
    return out, count


def python_tree(root):
    """For --python-baseline: a copy of the preserved Python app (server and raw web files) and its config, so the
    Python server runs as the tests expect without writing anything into reference/."""
    out = root / "python-tree"
    shutil.copytree(REPO / "reference/python/roadviewer/app", out / "roadviewer/app",
                    ignore=shutil.ignore_patterns("__pycache__"))
    shutil.copy(REPO / "reference/python/roadviewer/config.yaml.reference", out / "roadviewer/config.yaml")
    return out, 0


def tool_versions(node):
    pw = json.loads((HERE / "node_modules/playwright-core/package.json").read_text())["version"]
    browsers = json.loads((HERE / "node_modules/playwright-core/browsers.json").read_text())["browsers"]
    shell = next(b for b in browsers if b["name"] == "chromium-headless-shell")
    return {"node": subprocess.run([node, "--version"], capture_output=True, text=True).stdout.strip(),
            "playwright": pw, "chromium_headless_shell_revision": shell["revision"],
            "chromium_version": shell.get("browserVersion")}


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--supervise", help=argparse.SUPPRESS)
    p.add_argument("--hars-bin")
    p.add_argument("--web", default=str(WEB))
    p.add_argument("--out", help="artifact folder (created)")
    p.add_argument("--node", default=shutil.which("node"))
    p.add_argument("--jobs", type=int, default=1)
    p.add_argument("--repeat", type=int, default=1)
    p.add_argument("--timeout", type=int, default=900, help="seconds per test run (run_group.cjs uses 900)")
    p.add_argument("--only", nargs="*", help="labels or file names (maps.cjs selects its three entries)")
    p.add_argument("--isolate", choices=["on", "off"], default="on",
                   help="on: loopback-only network namespace per test (unshare -rn); off: no network isolation")
    p.add_argument("--pw-debug", action="store_true", help="DEBUG=pw:api in each test log")
    p.add_argument("--python-baseline", metavar="PYTHON",
                   help="run against the preserved Python server with this interpreter (Flask installed) instead of HARS")
    args = p.parse_args()
    if args.supervise:
        sys.exit(supervise(json.loads(args.supervise)))
    if not ((args.hars_bin or args.python_baseline) and args.out and args.node):
        p.error("--hars-bin (or --python-baseline), --out and node (on PATH or --node) are required")
    args.hars_bin = args.hars_bin or "-"
    if args.jobs < 1 or args.jobs * PORTS_PER_SLOT > MIRROR_PORTS[0] - PORT_FIRST:
        p.error(f"--jobs must be 1..{(MIRROR_PORTS[0] - PORT_FIRST) // PORTS_PER_SLOT}")
    args.hars_bin = str(Path(args.hars_bin).resolve())
    args.node = str(Path(args.node).resolve())
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    if args.isolate == "on" and subprocess.run(["unshare", "-rn", "true"], capture_output=True).returncode:
        sys.exit("unshare -rn is not permitted here; use --isolate off only where outside requests are blocked otherwise")
    subreaper()
    selected = [e for e in entries() if not args.only or e["label"] in args.only or e["file"] in args.only]
    root = Path(tempfile.mkdtemp(prefix="hars-browser-", dir=out))
    runner = root / "runner.py"
    shutil.copy(Path(__file__).resolve(), runner)
    os.environ["HARS_BROWSER_RUNNER_HOME"] = str(HERE)
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
    mirror_root, mirrored = python_tree(root) if args.python_baseline else start_mirror(args, root)
    slots = list(range(args.jobs))
    free_slots = list(slots)
    results = []

    def run(entry, iteration):
        if stop.is_set():
            return None
        with lock:
            slot = free_slots.pop()
        try:
            first = PORT_FIRST + slot * PORTS_PER_SLOT
            artifacts = out / f"run{iteration}" / entry["label"]
            artifacts.mkdir(parents=True, exist_ok=True)
            spec = {"label": entry["label"], "file": entry["file"], "env": entry["env"], "artifacts": str(artifacts),
                    "work": str(root / f"work-{iteration}-{slot}"), "mirror": str(mirror_root), "node": args.node,
                    "hars_bin": args.hars_bin, "web": args.web, "ports": f"{first}-{first + PORTS_PER_SLOT - 1}",
                    "timeout": args.timeout, "isolate": args.isolate == "on", "pw_debug": args.pw_debug,
                    "python_baseline": args.python_baseline}
            command = [sys.executable, str(runner), "--supervise", json.dumps(spec)]
            if spec["isolate"]:
                command = ["unshare", "-rn", "--", *command]
            with lock:
                if stop.is_set():
                    return None
                proc = subprocess.Popen(command, start_new_session=True, stdin=subprocess.DEVNULL)
                running[entry["label"], iteration] = proc
            try:
                proc.wait(timeout=args.timeout + 60)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            with lock:
                running.pop((entry["label"], iteration), None)
            try:
                result = json.loads((artifacts / "result.json").read_text())
            except (OSError, ValueError):
                result = {"label": entry["label"], "file": entry["file"], "env": entry["env"],
                          "outcome": "supervisor-failed", "supervisor_exit": proc.returncode}
            result.update(iteration=iteration, group=entry["group"], **static_counts(TESTS / entry["file"]))
            tail = (artifacts / "test.log").read_text(errors="replace")[-600:] if (artifacts / "test.log").exists() else ""
            print(f"[{iteration}] {result['outcome']:>8} {result.get('seconds', '-'):>6}s  {entry['label']}"
                  + ("" if result["outcome"] == "pass" else "\n    " + tail.strip().replace("\n", "\n    ")[-400:]),
                  flush=True)
            return result
        finally:
            with lock:
                free_slots.append(slot)

    try:
        with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
            for iteration in range(1, args.repeat + 1):
                for r in pool.map(lambda e: run(e, iteration), selected):
                    if r:
                        results.append(r)
                if stop.is_set():
                    break
    finally:
        leftovers = kill_tree()
        shutil.rmtree(root, ignore_errors=True)
    classification = {}
    if (HERE / "classification.json").exists():
        classification = json.loads((HERE / "classification.json").read_text())
    for r in results:
        r["classification"] = classification.get(r["label"])
    summary = {"started": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
               "seconds": round(time.monotonic() - began, 1), "jobs": args.jobs, "repeat": args.repeat,
               "isolate": args.isolate, "hars_bin": args.hars_bin, "mirrored_files": mirrored,
               "interrupted": stop.is_set(), "runner_leftover_processes": [c for _, c in leftovers],
               "temp_root_removed": not root.exists(), **tool_versions(args.node),
               "totals": {k: sum(1 for r in results if r["outcome"] == k) for k in sorted({r["outcome"] for r in results})},
               "results": results}
    (out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=1))
    print(json.dumps({k: summary[k] for k in ("seconds", "totals", "interrupted", "runner_leftover_processes",
                                              "temp_root_removed")}, ensure_ascii=False))
    return 130 if stop.is_set() else 0 if all(r["outcome"] == "pass" for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())

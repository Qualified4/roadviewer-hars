#!/usr/bin/env python3
"""RV_PYTHON for the browser tests (tests/browser/*.cjs): runs the test's Python program unchanged
with pyshim/ first on the module path, so its `import server` and `werkzeug.serving.make_server` start HARS on RV_DATA
(pyshim/hars_shim.py) instead of the Python server. A program that needs other Python Road Viewer modules fails with
ImportError, as it should: those are Python internals HARS does not have.
HARS_SHIM_PYTHON selects the interpreter (default: this one)."""
import os, sys
from pathlib import Path

shim = str(Path(__file__).resolve().parent / "pyshim")
os.environ["PYTHONPATH"] = os.pathsep.join(filter(None, [shim, os.environ.get("PYTHONPATH")]))
python = os.environ.get("HARS_SHIM_PYTHON", sys.executable)
os.execv(python, [python, *sys.argv[1:]])

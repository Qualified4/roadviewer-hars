"""Resolve the server adapter before any paths the test programs prepend to sys.path."""
import importlib.machinery, sys
from pathlib import Path

_HERE = str(Path(__file__).resolve().parent)
_NAMES = {"server", "hars_shim", "werkzeug", "werkzeug.serving", "werkzeug.middleware", "werkzeug.middleware.dispatcher"}


class _ShimFinder:
    @staticmethod
    def find_spec(name, path=None, target=None):
        if name not in _NAMES:
            return None
        return importlib.machinery.PathFinder.find_spec(name, [str(Path(_HERE, *name.split(".")[:-1]))])


sys.meta_path.insert(0, _ShimFinder())

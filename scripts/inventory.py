"""Extract the preserved Python contract without importing production modules."""
import ast
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / "reference/python/roadviewer/app"
modules, routes = [], []
for path in sorted(BASE.glob("*.py")):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    modules.append({"module": path.name, "lines": len(path.read_text(encoding="utf-8").splitlines()),
                    "symbols": [n.name for n in tree.body if isinstance(n, (ast.ClassDef, ast.FunctionDef))]})
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if node.func.attr in ("route", "add_url_rule") and node.args and isinstance(node.args[0], ast.Constant):
                methods = next((ast.literal_eval(k.value) for k in node.keywords if k.arg == "methods"), ["GET"])
                routes.append({"path": node.args[0].value, "methods": methods, "source": path.name, "line": node.lineno})
        # DeviceAPI registers a literal table in a loop.
        if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "routes" for t in node.targets) and isinstance(node.value, ast.List):
            for row in node.value.elts:
                if isinstance(row, ast.Tuple) and isinstance(row.elts[0], ast.Constant):
                    routes.append({"path": row.elts[0].value, "methods": ast.literal_eval(row.elts[-1]), "source": path.name, "line": row.lineno})
out = {"python_commit": "71add1a04ed505fc2cb6982052db4a9945d0808f", "python_version": "1.3.16",
       "uploader_commit": "5c2a8c7f6e14dac82678a390e408ef7a9c73eefa", "modules": modules,
       "routes": sorted(routes, key=lambda r: r["path"]),
       "tests": sorted(str(p.relative_to(ROOT / "reference/python")) for p in (ROOT / "reference/python/tests").glob("*" ) if p.is_file()),
       "note": "정적 추출. 동적 라우트와 조건부 응답 의미는 원본 구현/테스트를 함께 확인한다."}
(ROOT / "docs/hars/inventory.json").write_text(json.dumps(out, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(f"Inventory: {len(modules)} modules, {len(routes)} route entries, {len(out['tests'])} test files")

# Compile only original pure field declarations; never import/run the application.
env = {}
telemetry_tree = ast.parse((BASE / "telemetry.py").read_text(encoding="utf-8"))
fields = next(n for n in telemetry_tree.body if isinstance(n, ast.Assign) and any(isinstance(t,ast.Name) and t.id == "FIELDS" for t in n.targets))
exec(compile(ast.Module(body=[fields],type_ignores=[]),"telemetry-fields","exec"),env)
decoder_tree = ast.parse((BASE / "decoder.py").read_text(encoding="utf-8"))
declarations = [n for n in decoder_tree.body if
                isinstance(n, ast.FunctionDef) and n.name in ("spec","telemetry_paths") or
                isinstance(n, ast.Assign) and any(isinstance(t,ast.Name) and t.id in ("OUTPUT","SPECS") for t in n.targets)]
exec(compile(ast.Module(body=declarations,type_ignores=[]),"decoder-fields","exec"),env)
specs = env["SPECS"]
specs["carState"]["vehicleGps"] = None
specs["carControl"]["hudControl"] = {"leadDistanceBars":None}
for topic in ("roadEncodeIdx","wideRoadEncodeIdx"):
    specs[topic] = specs["qRoadEncodeIdx"]
resources = ROOT / "resources"
resources.mkdir(exist_ok=True)
(resources / "log-fields.json").write_text(json.dumps(specs,ensure_ascii=False,indent=2) + "\n",encoding="utf-8")
(resources / "telemetry-fields.json").write_text(json.dumps(env["FIELDS"],ensure_ascii=False,indent=2) + "\n",encoding="utf-8")

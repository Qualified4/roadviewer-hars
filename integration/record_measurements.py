"""Record the existing one-shot measurement, without rerunning workloads."""
from pathlib import Path
import argparse
import datetime
import json
import subprocess

p=argparse.ArgumentParser()
p.add_argument("--commit",required=True,help="revision that was actually compiled into the measured Rust binary")
args=p.parse_args()
root=Path(__file__).resolve().parents[1]
report=json.loads((root/"artifacts/api-benchmark.json").read_text())
report["rust_commit"]=subprocess.check_output(["git","rev-parse",args.commit+"^{commit}"],cwd=root,text=True).strip()
report["python_commit"]="71add1a04ed505fc2cb6982052db4a9945d0808f"
report["recorded_at_utc"]=datetime.datetime.now(datetime.timezone.utc).isoformat()
folder=root/"docs/hars/measurements"
folder.mkdir(exist_ok=True)
(folder/"api-2026-11-15.json").write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
print("Recorded existing measurement; no additional benchmark run")

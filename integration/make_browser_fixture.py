"""Small valid Cap'n Proto/Zstd fixture, with no real driving or credential data."""
import argparse
from pathlib import Path
import shutil
import tempfile
import capnp
import zstandard

p=argparse.ArgumentParser()
p.add_argument("output")
args=p.parse_args()
root=Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="hars-schema-") as temp:
    schema=Path(temp)/"cereal"
    shutil.copytree(root/"reference/python/roadviewer/schema/cereal",schema)
    log=capnp.load(str(schema/"log.capnp"))
    frames=[]
    for i in range(2):
        event=log.Event.new_message()
        event.logMonoTime=1000000000+i*10000000
        event.valid=True
        event.init("carState").vEgo=10+i
        frames.append(event.to_bytes())
    output=Path(args.output)
    output.parent.mkdir(parents=True,exist_ok=True)
    output.write_bytes(zstandard.ZstdCompressor().compress(b"".join(frames)))
print("Synthetic log fixture generated")

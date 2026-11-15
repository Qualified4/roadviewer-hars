"""CI synthetic oracle for complete log + H.264 TS + two raw HEVC conversion.

This is separate from the private real-corpus reports and must never be labelled
real driving data. Encoding here creates test inputs; product conversion copies
the compressed pictures without encoding them.
"""
import argparse
import gzip
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zstandard
from real_replay_compare import differences


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary",type=Path)
    args=parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hars-pipeline-") as work:
        work=Path(work);reference=work/"reference"
        shutil.copytree(Path(__file__).resolve().parents[1]/"reference/python/roadviewer",reference)
        sys.path.insert(0,str(reference/"app"))
        from decoder import log
        source=work/"inputs";source.mkdir()
        common=["ffmpeg","-v","error","-nostdin","-f","lavfi","-i","testsrc2=size=320x240:rate=20","-t","6","-threads","2","-an"]
        subprocess.run(common+["-c:v","libx264","-preset","ultrafast","-bf","0","-g","20","-f","mpegts",str(source/"qcamera.ts")],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
        subprocess.run(common+["-c:v","libx265","-preset","ultrafast","-x265-params","pools=1:frame-threads=1:bframes=0:keyint=20","-f","hevc",str(source/"fcamera.hevc")],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
        shutil.copyfile(source/"fcamera.hevc",source/"ecamera.hevc")
        events=[]
        def emit(kind,value,stamp=10_000_000_000,valid=True):
            e=log.Event.new_message(logMonoTime=stamp,valid=valid)
            e.from_dict({kind:value});events.append((stamp,e.to_bytes()))
        emit("initData",{"dongleId":"synthetic","wallTimeNanos":1700000000000000000},1_000_000_000)
        emit("clocks",{"wallTimeNanos":1700000009000000000})
        emit("carParams",{"brand":"hyundai","maxLateralAccel":3.})
        emit("deviceState",{"deviceType":"tici"})
        for camera in ("roadCameraState","wideRoadCameraState"):emit(camera,{"sensor":"ar0231"})
        emit("liveCalibration",{"calStatus":"calibrated","rpyCalib":[.01,.02,.03],"wideFromDeviceEuler":[0.,.01,.02],"height":[1.22]})
        for i in range(120):
            stamp=10_000_000_000+i*50_000_000;frame=10000+i
            line=lambda y:{"x":[0.,10.,30.],"y":[y,y+.1,y+.3],"z":[0.,0.,0.]}
            emit("carState",{"vEgo":20.,"aEgo":.2,"steeringAngleDeg":12.,"steeringPressed":i>90,"leftBlinker":20<i<40,"leftBlindspot":30<i<60,"vehicleGps":{"available":True,"valid":True,"sampleMonoTime":stamp,"latitude":37.+i*.000005,"longitude":127.,"headingValid":True,"headingDeg":0.}},stamp)
            emit("carControl",{"enabled":i<90,"latActive":i<90,"longActive":i<90,"hudControl":{"leadDistanceBars":2}},stamp)
            emit("carOutput",{"actuatorsOutput":{"torque":.8,"accel":.4}},stamp)
            emit("controlsState",{"desiredCurvature":.01,"activeLaneLine":True,"lateralControlState":{"torqueState":{"active":True}}},stamp)
            emit("longitudinalPlan",{"desiredDistance":30.},stamp)
            for topic in ("qRoadEncodeIdx","roadEncodeIdx","wideRoadEncodeIdx"):emit(topic,{"frameId":frame,"timestampEof":stamp,"segmentId":i},stamp)
            emit("modelV2",{"frameId":frame,"timestampEof":stamp,"position":line(0.),"laneLines":[line(y) for y in (-5.,-1.5,1.5,5.)],"roadEdges":[line(-6.),line(6.)],"laneLineProbs":[.1,.9,.9,.1],"roadEdgeStds":[.2,.2],"leadsV3":[{"x":[25.],"y":[.2],"v":[18.],"prob":.9}]},stamp+20_000_000)
        with (source/"rlog.zst").open("wb") as raw,zstandard.ZstdCompressor().stream_writer(raw) as compressed:
            for _,event in sorted(events,key=lambda row:row[0]):compressed.write(event)
        env=dict(os.environ,RV_PREPARED_DIR=str(work/"python"))
        with (work/"python.log").open("wb") as output:
            subprocess.run([sys.executable,str(reference/"app/decoder.py"),str(source/"rlog.zst"),"synthetic"],env=env,stdout=output,stderr=output,check=True,timeout=60)
        with (work/"rust.log").open("wb") as output:
            subprocess.run([str(args.binary),"convert-log",str(source/"rlog.zst"),str(work/"rust"),"synthetic"],stdout=output,stderr=output,check=True,timeout=60)
        for name in ("data.json.gz","telemetry.json.gz","track.json.gz","summary.json"):
            def load(p):
                with (gzip.open(p,"rt") if p.suffix==".gz" else p.open()) as f:return json.load(f)
            a,b=load(work/"python"/name),load(work/"rust"/name)
            if name=="data.json.gz":
                a.pop("key");b.pop("key")
                assert set(a["videos"])==set(b["videos"])=={"qcamera","front","wide"}
                assert len(b["frames"])==120 and not b["warnings"]
            error=differences(a,b)
            assert not error,{"file":name,"differences":dict(error)}
        assert (work/"rust/thumbnail.jpg").read_bytes().startswith(b"\xff\xd8")
        print(json.dumps({"synthetic":True,"model_frames":120,"cameras":3,"full_json_equivalence":True,"lossless_native_remux":True}))


if __name__=="__main__":main()

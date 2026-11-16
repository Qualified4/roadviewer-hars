"""Size report of a built HARS image, as JSON on stdout (standard library only).

    python3 scripts/image_report.py TAG [--compare OTHER_TAG] [--binary PATH]

Reports the uncompressed size (`docker image inspect`), a transfer-like
compressed size (each layer's uncompressed tar gzip -6, summed), the layer
blobs as `docker save` wrote them (with the containerd image store these are
the registry's gzip blobs, the pushed size), `docker save | gzip -6` as a
whole for reference, every layer, the Rust binary (size, symbol table,
debug sections, dynamic links) and the FFmpeg files and packages the image
holds. With --compare, the packages and sizes TAG has beyond OTHER_TAG, which
measures what removing a package really saves image for image: build OTHER_TAG
from the same Dockerfile without that package (its binary need not run), and the
difference counts the package and every dependency only it pulled in, by image
size and by the bytes of the files those packages own.

Needs docker and the images locally; no network, no registry. The JSON holds
no host path, clock or file name beyond the image's own.
"""
import argparse
import gzip
import json
import re
import struct
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

FFMPEG_FILE = re.compile(r"(^|/)(ffmpeg|ffprobe|ffplay)$|(^|/)lib(av(codec|format|util|device|filter)|sw(scale|resample)|postproc)\.so")
FFMPEG_PACKAGE = re.compile(r"^(ffmpeg|libav(codec|format|util|device|filter)\d*|libsw(scale|resample)\d*|libpostproc\d*)(:|$)")


def run(*args, **kw):
    return subprocess.run(args, check=True, capture_output=True, **kw).stdout


class Counter:
    def __init__(self):
        self.n = 0

    def write(self, b):
        self.n += len(b)
        return len(b)

    def flush(self):
        pass


def gzip_size(stream):
    sink = Counter()
    with gzip.GzipFile(fileobj=sink, mode="wb", compresslevel=6, mtime=0) as z:
        while chunk := stream.read(1 << 20):
            z.write(chunk)
    return sink.n


def layers(tag):
    """(uncompressed tar bytes, gzip -6 bytes, created_by) per layer, base first."""
    with tempfile.TemporaryDirectory(prefix="hars-image-") as temp:
        saved = Path(temp) / "image.tar"
        with saved.open("wb") as out:
            subprocess.run(["docker", "save", tag], check=True, stdout=out)
        with saved.open("rb") as whole:
            total_gzip = gzip_size(whole)
        rows = []
        with tarfile.open(saved) as tar:
            manifest = json.load(tar.extractfile("manifest.json"))[0]
            config = json.load(tar.extractfile(manifest["Config"]))
            history = [h.get("created_by", "") for h in config.get("history", []) if not h.get("empty_layer")]
            for i, name in enumerate(manifest["Layers"]):
                member = tar.getmember(name)
                with tar.extractfile(member) as layer:
                    magic = layer.read(4)
                layer = tar.extractfile(member)
                # The containerd image store saves layers as their registry blobs (gzip); older stores save tars.
                media = "gzip" if magic[:2] == b"\x1f\x8b" else "zstd" if magic == b"\x28\xb5\x2f\xfd" else "tar"
                counted = Counter()
                if media == "gzip":
                    source = Tee(gzip.GzipFile(fileobj=layer), counted)
                elif media == "tar":
                    source = Tee(layer, counted)
                else:
                    source = None
                compressed = gzip_size(source) if source else None
                rows.append({"index": i, "blob_bytes": member.size, "blob_media": media,
                             "bytes": counted.n if source else None, "gzip6_bytes": compressed,
                             "created_by": (history[i] if i < len(history) else "")[:160]})
                layer.close()
        return rows, saved_size(saved), total_gzip


class Tee:
    def __init__(self, inner, counter):
        self.inner, self.counter = inner, counter

    def read(self, n):
        b = self.inner.read(n)
        self.counter.write(b)
        return b


def saved_size(path):
    return path.stat().st_size


def elf_report(path):
    data = path.read_bytes()
    report = {"bytes": len(data)}
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        report["elf"] = False
        return report
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
    sections = [struct.unpack_from("<IIQQQQ", data, shoff + i * shentsize) for i in range(shnum)]
    names_at = sections[shstrndx][4]
    def name(offset):
        end = data.index(b"\0", names_at + offset)
        return data[names_at + offset:end].decode()
    named = {name(s[0]): s[5] for s in sections}
    debug = {k: v for k, v in named.items() if k.startswith(".debug")}
    report.update({"elf": True, "symtab": ".symtab" in named, "debug_section_bytes": sum(debug.values()),
                   "debug_sections": sorted(debug), "stripped": ".symtab" not in named and not debug})
    return report


def in_container(tag, script):
    return run("docker", "run", "--rm", "--network", "none", "--entrypoint", "sh", tag, "-c", script).decode()


def packages(tag):
    out = in_container(tag, "dpkg-query -W -f '${Package}\\t${Installed-Size}\\n'")
    return {p: int(s or 0) for p, s in (line.split("\t") for line in out.splitlines() if line)}


def package_files(tag, names):
    """Bytes of the regular files each package owns in TAG (not dpkg's Installed-Size estimate),
    and the shared libraries among them."""
    script = ('for p in "$@"; do dpkg -L "$p" 2>/dev/null | while read -r f; do '
              'if [ -f "$f" ] && [ ! -L "$f" ]; then stat -c "%s|%i|$p|%n" "$f"; fi; done; done; exit 0')
    out = run("docker", "run", "--rm", "--network", "none", "--entrypoint", "sh", tag, "-c", script, "sh",
              *names).decode()
    per, libraries, seen = {}, {}, set()
    for line in out.splitlines():
        size, inode, package, path = line.split("|", 3)
        # Hard links (Mesa's DRI drivers are one file under many names) take their bytes once.
        if inode in seen:
            continue
        seen.add(inode)
        per[package] = per.get(package, 0) + int(size)
        if re.search(r"\.so(\.\d+)*$", path):
            libraries[path.rsplit("/", 1)[-1]] = int(size)
    return per, libraries


def report(tag, binary):
    inspect = json.loads(run("docker", "image", "inspect", tag))[0]
    rows, saved, saved_gzip = layers(tag)
    with tempfile.TemporaryDirectory(prefix="hars-binary-") as temp:
        container = run("docker", "create", tag).decode().strip()
        try:
            run("docker", "cp", f"{container}:{binary}", f"{temp}/binary")
        finally:
            run("docker", "rm", container)
        elf = elf_report(Path(temp) / "binary")
    ldd = in_container(tag, f"ldd {binary} || true").splitlines()
    links = [line.strip().split()[0] for line in ldd if line.strip()]
    files = in_container(tag, "find / -xdev \\( -type f -o -type l \\) 2>/dev/null | grep -v '^/proc' || true").splitlines()
    ffmpeg_files = sorted(f for f in files if FFMPEG_FILE.search(f))
    size_of = in_container(tag, "for f in $(find / -xdev -type f \\( -name 'ffmpeg' -o -name 'ffprobe' -o -name 'libav*.so*' -o -name 'libsw*.so*' -o -name 'libpostproc*.so*' \\) 2>/dev/null); do stat -c '%s %n' \"$f\"; done").split()
    pkgs = packages(tag)
    return {
        "tag": tag,
        "architecture": inspect.get("Architecture"),
        # The unpacked layers, the size a host stores. `docker image inspect`'s Size is kept apart:
        # with the containerd image store it also counts the compressed blobs.
        "uncompressed_bytes": sum(r["bytes"] or 0 for r in rows),
        "docker_inspect_size_bytes": inspect["Size"],
        "compressed_layers_gzip6_bytes": sum(r["gzip6_bytes"] or r["blob_bytes"] for r in rows),
        "layer_blobs_bytes": sum(r["blob_bytes"] for r in rows),
        "docker_save_bytes": saved,
        "docker_save_gzip6_bytes": saved_gzip,
        "layers": rows,
        "binary": {"path": binary, **elf},
        "binary_dynamic_links": links,
        "binary_ffmpeg_links": [l for l in links if FFMPEG_FILE.search("/" + l)],
        "ffmpeg_files": ffmpeg_files,
        "ffmpeg_file_bytes": sum(int(size_of[i]) for i in range(0, len(size_of), 2)),
        "ffmpeg_packages": sorted(p for p in pkgs if FFMPEG_PACKAGE.search(p)),
        "packages": len(pkgs),
        "packages_installed_kib": sum(pkgs.values()),
    }


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("tag")
    p.add_argument("--compare", help="an image built without some packages: what TAG has beyond it")
    p.add_argument("--binary", default="/usr/local/bin/roadviewer-hars")
    args = p.parse_args()
    out = report(args.tag, args.binary)
    if args.compare:
        other = report(args.compare, args.binary)
        mine, theirs = packages(args.tag), packages(args.compare)
        extra = {k: v for k, v in mine.items() if k not in theirs}
        files, libraries = package_files(args.tag, sorted(extra))
        sizes = ("uncompressed_bytes", "docker_inspect_size_bytes", "compressed_layers_gzip6_bytes",
                 "layer_blobs_bytes", "docker_save_gzip6_bytes")
        out["compare"] = {
            "tag": args.compare,
            **{k: other[k] for k in sizes},
            **{f"saved_{k}": out[k] - other[k] for k in sizes},
            **{f"saved_{k}_percent": round(100 * (out[k] - other[k]) / out[k], 1) for k in sizes},
            "binary_ffmpeg_links": other["binary_ffmpeg_links"],
            "packages_only_in_tag_file_bytes": sum(files.values()),
            "packages_only_in_tag_shared_libraries": len(libraries),
            "packages_only_in_tag_shared_library_bytes": sum(libraries.values()),
            "largest_packages_only_in_tag": sorted(({"package": k, "file_bytes": v} for k, v in files.items()),
                                                   key=lambda r: -r["file_bytes"])[:25],
            "packages_only_in_tag": sorted(extra),
            "packages_only_in_tag_count": len(extra),
            "packages_only_in_tag_installed_kib": sum(extra.values()),
            "layers": other["layers"],
        }
    json.dump(out, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()

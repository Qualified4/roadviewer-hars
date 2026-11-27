"""Extract a single version's release notes from roadviewer_hars/CHANGELOG.md."""
import argparse
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CHANGELOG = ROOT / "roadviewer_hars/CHANGELOG.md"
LEVEL2_HEADING_RE = re.compile(r"^##[ \t]+(.+?)[ \t]*$")


def extract_section(changelog_text: str, tag: str) -> str:
    """Return the markdown body for `tag` (`## <tag>` or `## v<tag>`), or raise ValueError."""
    raw_tag = tag.strip()
    version = raw_tag[1:] if raw_tag.startswith("v") else raw_tag
    if not version:
        raise ValueError("Release tag must not be empty")

    target_re = re.compile(rf"^v?{re.escape(version)}(?:[ \t]+.*)?$")
    lines = changelog_text.splitlines()
    matches: list[int] = []
    for idx, line in enumerate(lines):
        m = LEVEL2_HEADING_RE.match(line)
        if m and target_re.match(m.group(1)):
            matches.append(idx)

    if not matches:
        raise ValueError(f"Release section '## {version}' not found in changelog")
    if len(matches) > 1:
        raise ValueError(f"Duplicate release section '## {version}' found in changelog")

    start = matches[0] + 1
    end = len(lines)
    for idx in range(start, len(lines)):
        if LEVEL2_HEADING_RE.match(lines[idx]):
            end = idx
            break

    body_lines = lines[start:end]
    while body_lines and not body_lines[0].strip():
        body_lines.pop(0)
    while body_lines and not body_lines[-1].strip():
        body_lines.pop()

    if not body_lines:
        raise ValueError(f"Release section '## {version}' is empty in changelog")

    return "\n".join(body_lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Extract per-version release notes from CHANGELOG.md")
    parser.add_argument("--tag", required=True, help="Release tag (e.g. 2.1.6 or v2.1.6)")
    parser.add_argument("--changelog", type=Path, default=DEFAULT_CHANGELOG, help="Path to CHANGELOG.md")
    parser.add_argument("--output", type=Path, help="Optional output file path; defaults to stdout")
    args = parser.parse_args(argv)

    try:
        changelog_text = args.changelog.read_text(encoding="utf-8")
        notes = extract_section(changelog_text, args.tag)
    except (OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if args.output:
        args.output.write_text(notes, encoding="utf-8")
    else:
        sys.stdout.write(notes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

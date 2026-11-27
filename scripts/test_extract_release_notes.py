"""Unit tests for scripts/extract_release_notes.py."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.extract_release_notes import DEFAULT_CHANGELOG, extract_section

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/extract_release_notes.py"


class ExtractReleaseNotesTests(unittest.TestCase):
    def test_real_changelog_versions(self) -> None:
        text = DEFAULT_CHANGELOG.read_text(encoding="utf-8")

        notes_216 = extract_section(text, "2.1.6")
        self.assertIn("재생 미니맵의 둥근 직사각형 페이드", notes_216)
        self.assertNotIn("## 2.1.6", notes_216)
        self.assertNotIn("## 2.1.5", notes_216)

        notes_210 = extract_section(text, "2.1.0")
        self.assertIn("테슬라 스타일 3D 페이드 미니맵", notes_210)
        self.assertNotIn("## 2.0.26", notes_210)

        notes_200 = extract_section(text, "2.0.0")
        self.assertIn("Rust로 다시 구현한 Road Viewer의 첫 정식 버전", notes_200)
        self.assertIn("### 1.x에서 옮겨 오는 경우", notes_200)
        self.assertNotIn("## 1.3.18", notes_200)

        # Ensure exact version boundary (2.0.1 must not match 2.0.10 or 2.0.19).
        notes_201 = extract_section(text, "2.0.1")
        self.assertIn("마지막으로 고른 화질 유지", notes_201)
        self.assertNotIn("썸네일을 만들 메모리가 부족", notes_201)

    def test_v_prefix_matching(self) -> None:
        sample = "## v2.2.0\n\n- Added feature A\n\n## 2.1.0\n\n- Old feature\n"
        self.assertEqual(extract_section(sample, "2.2.0"), "- Added feature A\n")
        self.assertEqual(extract_section(sample, "v2.2.0"), "- Added feature A\n")
        self.assertEqual(extract_section(sample, "v2.1.0"), "- Old feature\n")

    def test_missing_or_empty_section_fails(self) -> None:
        sample = "## 2.1.0\n\n   \n\n## 2.0.0\n\n- Initial\n"
        with self.assertRaises(ValueError):
            extract_section(sample, "2.1.0")
        with self.assertRaises(ValueError):
            extract_section(sample, "9.9.9")

    def test_duplicate_section_fails(self) -> None:
        sample = "## 2.1.0\n\n- First\n\n## 2.1.0\n\n- Second\n"
        with self.assertRaises(ValueError):
            extract_section(sample, "2.1.0")

    def test_cli_output_and_nonzero_exit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out_path = Path(tmp) / "notes.md"
            ok = subprocess.run(
                [sys.executable, str(SCRIPT), "--tag", "2.1.6", "--output", str(out_path)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(ok.returncode, 0, ok.stderr)
            self.assertIn("재생 미니맵", out_path.read_text(encoding="utf-8"))

            missing = subprocess.run(
                [sys.executable, str(SCRIPT), "--tag", "9.9.9"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("not found", missing.stderr)


if __name__ == "__main__":
    unittest.main()

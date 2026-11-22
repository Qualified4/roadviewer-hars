#!/usr/bin/env python3
"""Check the browser manifest and verdict rules without launching HARS or a browser.
Run with python3 tests/browser/test_gate_rules.py.
"""
import copy, sys, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_browser_tests as rb  # noqa: E402

EXPECTATIONS = rb.load_expectations()
LABELS = [e["label"] for e in rb.entries()]


def result(label="tags", outcome="pass", **extra):
    failure = {"errors": 0} if outcome == "pass" else {"errors": 1, "error": "AssertionError", "message": "x", "line": 1}
    return {"label": label, "outcome": outcome, "failure": failure, "leftover_processes": [],
            "shim": {"restarts": 0, "refused_restarts": 0}, **extra}


def full(**changes):
    rows = [result(label) for label in LABELS]
    for label, row in changes.items():
        rows = [row if r["label"] == label else r for r in rows]
    return rows


class GateData(unittest.TestCase):
    def test_only_the_many_leak_entry_may_restart(self):
        self.assertEqual(EXPECTATIONS["restart_allowed"], ["maps[map_parts=many,leak]"])

    def test_no_other_kind_of_exception(self):
        self.assertEqual(set(EXPECTATIONS), {"about", "entries", "restart_allowed"})
        for kind in ("excluded", "expected_failures", "known_upstream_defects", "hars_checks"):
            e = copy.deepcopy(EXPECTATIONS)
            e[kind] = {}
            self.assertIn("unknown keys", rb.expectation_problem(e))

    def test_entry_count_is_the_manifest(self):
        self.assertEqual(EXPECTATIONS["entries"], len(LABELS))
        self.assertEqual(len(set(LABELS)), len(LABELS))
        for e in rb.entries():
            self.assertTrue((rb.TESTS / e["file"]).is_file(), e["file"])

    def test_fixtures_are_checked_in(self):
        # .gitignore has *.mp4: the fixtures were once copied but not committed. CI runs this in a fresh checkout.
        for f in rb.FIXTURES:
            self.assertTrue((rb.TESTS / f).is_file(), f)

    def test_ci_runs_every_group_once(self):
        import json, re
        ci = (rb.REPO / ".github/workflows/ci.yml").read_text()
        shards = re.findall(r"\{shard: (\d+), groups: ([^}]+)\}", ci)
        groups = [g for _, gs in shards for g in gs.split()]
        manifest = list(json.loads((rb.TESTS / "browser-groups.json").read_text()))
        self.assertEqual(sorted(groups), sorted(manifest))
        self.assertEqual([int(n) for n, _ in shards], list(range(1, len(shards) + 1)))

    def test_no_rerun_option(self):
        import inspect
        source = inspect.getsource(rb)
        for word in ("attempt", "--repeat", "earlier_attempts", "variant"):
            self.assertNotIn(word, source)


class Judge(unittest.TestCase):
    def test_pass_and_fail(self):
        self.assertEqual(rb.judge(result(), EXPECTATIONS), "pass")
        for outcome in ("fail", "timeout", "supervisor-failed", "isolation-failed", "interrupted"):
            self.assertEqual(rb.judge(result(outcome=outcome), EXPECTATIONS), "fail", outcome)

    def test_leftover_process_fails_a_pass(self):
        self.assertEqual(rb.judge(result(leftover_processes=["node x"]), EXPECTATIONS), "fail")

    def test_restart_only_where_allowed(self):
        restarted = {"restarts": 1, "refused_restarts": 0}
        self.assertEqual(rb.judge(result("maps[map_parts=many,leak]", shim=restarted), EXPECTATIONS), "pass")
        self.assertEqual(rb.judge(result("tags", shim=restarted), EXPECTATIONS), "fail")
        refused = {"restarts": 0, "refused_restarts": 1}
        self.assertEqual(rb.judge(result("tags", shim=refused), EXPECTATIONS), "fail")
        self.assertEqual(rb.judge(result("maps[map_parts=many,leak]", shim=refused), EXPECTATIONS), "fail")


class Verdict(unittest.TestCase):
    def gate(self, rows, labels=LABELS, full_manifest=True):
        return rb.verdict(rows, labels, EXPECTATIONS, full_manifest)

    def test_whole_manifest_passes(self):
        g = self.gate(full())
        self.assertTrue(g["ok"], g["reasons"])
        self.assertEqual(g["counts"], {"pass": 50, "fail": 0})

    def test_missing_duplicate_unlisted_or_empty_run_fails(self):
        rows = full()
        self.assertFalse(self.gate(rows[1:])["ok"])
        self.assertFalse(self.gate(rows + rows[:1])["ok"])
        self.assertFalse(self.gate(rows + [result("unlisted")])["ok"])
        self.assertIn("no test ran", self.gate([], labels=["tags"], full_manifest=False)["reasons"])

    def test_one_failure_fails(self):
        self.assertFalse(self.gate(full(tags=result("tags", outcome="fail")))["ok"])

    def test_entry_count_must_match(self):
        e = dict(EXPECTATIONS, entries=49)
        self.assertFalse(rb.verdict(full(), LABELS, e, True)["ok"])


if __name__ == "__main__":
    unittest.main(verbosity=1)

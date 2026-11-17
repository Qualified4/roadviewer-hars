#!/usr/bin/env python3
"""Rules of the browser gate that a real run reaches only by chance, and the gate data itself: the excluded set is
exactly {replay_format2}, only maps[map_parts=many,leak] may restart HARS, and no other kind of exception (expected
failures, reruns) exists. No browser, no HARS; builds results the way the runner records them and calls the runner's
own functions. CI runs it in Browser checks.
  python3 integration/upstream_cjs/test_gate_rules.py"""
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
    rows = [result(label) for label in LABELS if label not in EXPECTATIONS["excluded"]]
    for label, row in changes.items():
        rows = [row if r["label"] == label else r for r in rows]
    return rows


class GateData(unittest.TestCase):
    def test_excluded_is_exactly_replay_format2(self):
        self.assertEqual(set(EXPECTATIONS["excluded"]), {"replay_format2"})
        self.assertFalse((rb.TESTS / "replay_format2.cjs").exists(), "replay_format2 is not imported")

    def test_only_the_many_leak_entry_may_restart(self):
        self.assertEqual(EXPECTATIONS["restart_allowed"], ["maps[map_parts=many,leak]"])

    def test_no_other_kind_of_exception(self):
        self.assertEqual(set(EXPECTATIONS), {"about", "entries", "excluded", "restart_allowed"})
        for kind in ("expected_failures", "known_upstream_defects", "hars_checks"):
            e = copy.deepcopy(EXPECTATIONS)
            e[kind] = {}
            self.assertIn("unknown keys", rb.expectation_problem(e))

    def test_entry_count_is_the_manifest(self):
        self.assertEqual(EXPECTATIONS["entries"], len(LABELS))
        self.assertEqual(len(set(LABELS)), len(LABELS))
        for e in rb.entries():
            if e["label"] not in EXPECTATIONS["excluded"]:
                self.assertTrue((rb.TESTS / e["file"]).is_file(), e["file"])

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
        return rb.verdict(rows, ["replay_format2"], labels, EXPECTATIONS, full_manifest)

    def test_whole_manifest_passes(self):
        g = self.gate(full())
        self.assertTrue(g["ok"], g["reasons"])
        self.assertEqual(g["counts"], {"pass": 50, "excluded": 1, "fail": 0})

    def test_missing_duplicated_excluded_run_or_nothing_fails(self):
        rows = full()
        self.assertFalse(self.gate(rows[1:])["ok"])
        self.assertFalse(self.gate(rows + rows[:1])["ok"])
        self.assertFalse(self.gate(rows + [result("replay_format2")])["ok"])
        self.assertIn("no test ran", self.gate([], labels=["replay_format2"], full_manifest=False)["reasons"])

    def test_one_failure_fails(self):
        self.assertFalse(self.gate(full(tags=result("tags", outcome="fail")))["ok"])

    def test_entry_count_must_match(self):
        e = dict(EXPECTATIONS, entries=50)
        self.assertFalse(rb.verdict(full(), ["replay_format2"], LABELS, e, True)["ok"])


if __name__ == "__main__":
    unittest.main(verbosity=1)

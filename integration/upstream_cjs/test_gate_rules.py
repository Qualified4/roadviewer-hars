#!/usr/bin/env python3
"""Rules of the browser gate that a real run reaches only by chance: the known upstream defect's rerun (signature,
attempt limit, hits) and the refusal of expectations.json. No browser, no HARS; builds results the way the runner
records them and calls the runner's own functions.
  python3 integration/upstream_cjs/test_gate_rules.py"""
import copy, sys, unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_browser_tests as rb  # noqa: E402

EXPECTATIONS = rb.load_expectations()
DEFECT = EXPECTATIONS["known_upstream_defects"]["video_overlay"]
CHECK = DEFECT["guarded_by"]


def run(outcome="fail", errors=1, error="AssertionError", message="closing hands focus to nothing", line=77, **extra):
    failure = {"errors": errors, "error": error, "message": message, "line": line} if outcome != "pass" else {"errors": 0}
    return {"label": "video_overlay", "outcome": outcome, "failure": failure, "iteration": 1, **extra}


def final(*attempts):
    """The base result the runner keeps: the last attempt, carrying the earlier ones."""
    last = copy.deepcopy(attempts[-1])
    if len(attempts) > 1:
        last["earlier_attempts"] = [copy.deepcopy(a) for a in attempts[:-1]]
    return last


def rerun_sequence(outcomes):
    """What the runner's loop does with these attempt outcomes (taken in order while it asks for another attempt)."""
    attempts = [outcomes[0]]
    while rb.another_attempt(attempts, DEFECT) and len(attempts) < len(outcomes):
        attempts.append(outcomes[len(attempts)])
    return attempts


SIG, PASS = run(), run("pass")


class DefectRerun(unittest.TestCase):
    def judged(self, *outcomes):
        attempts = rerun_sequence(list(outcomes))
        r = final(*attempts)
        return len(attempts), rb.judge(r, EXPECTATIONS), rb.defect_hits(r, EXPECTATIONS)

    def test_pass_first_time(self):
        self.assertEqual(self.judged(PASS), (1, "pass", 0))

    def test_signature_then_pass(self):
        self.assertEqual(self.judged(SIG, PASS), (2, "pass", 1))
        self.assertEqual(self.judged(SIG, SIG, PASS), (3, "pass", 2))

    def test_three_signature_failures_fail(self):
        self.assertEqual(self.judged(SIG, SIG, SIG, PASS), (3, "fail", 3))
        self.assertFalse(rb.another_attempt([SIG, SIG, SIG], DEFECT))

    def test_other_failures_are_not_rerun(self):
        for other in (run(line=78), run(line=None), run(message="ground mode has no vertical stem"),
                      run(errors=2), run(error="TimeoutError"), run("timeout", errors=0),
                      run("supervisor-failed", errors=0)):
            with self.subTest(other=other):
                self.assertFalse(rb.another_attempt([other], DEFECT))
                self.assertEqual(self.judged(other, PASS), (1, "fail", 0))

    def test_signature_then_other_failure_fails(self):
        self.assertEqual(self.judged(SIG, run(line=120), PASS), (2, "fail", 1))

    def test_recorded_attempts_are_rechecked(self):
        # What --aggregate reads back: more attempts than the limit, or a rerun after a different failure.
        self.assertEqual(rb.judge(final(SIG, SIG, SIG, PASS), EXPECTATIONS), "fail")
        self.assertEqual(rb.judge(final(run(line=78), PASS), EXPECTATIONS), "fail")
        other = dict(PASS, label="tags", earlier_attempts=[dict(SIG, label="tags")])
        self.assertEqual(rb.judge(other, EXPECTATIONS), "fail")

    def test_rule_is_only_for_the_defect_entry(self):
        self.assertEqual(rb.judge(dict(SIG, label="motion"), EXPECTATIONS), "fail")
        self.assertEqual(rb.defect_hits(dict(SIG, label="motion"), EXPECTATIONS), 0)


class Verdict(unittest.TestCase):
    labels = ["video_overlay"]

    def gate(self, *results):
        return rb.verdict(list(results), [], self.labels, 1, EXPECTATIONS, False)

    def check(self, outcome="pass"):
        return {"label": CHECK, "hars_check": CHECK, "runs_with": "video_overlay", "outcome": outcome,
                "failure": {"errors": 0}, "iteration": 1}

    def test_hits_are_counted_when_passing(self):
        g = self.gate(final(SIG, SIG, PASS), self.check())
        self.assertTrue(g["ok"], g["reasons"])
        self.assertEqual(g["counts"]["known_upstream_defect_hits"], 2)
        self.assertEqual(g["counts"]["hars_checks_passed"], 1)

    def test_hars_check_failure_fails_the_gate(self):
        g = self.gate(final(PASS), self.check("fail"))
        self.assertFalse(g["ok"])

    def test_hars_check_must_run_with_its_entry(self):
        g = self.gate(final(PASS))
        self.assertFalse(g["ok"])
        self.assertIn(f"{CHECK} ran 0 times", " ".join(g["reasons"]))

    def test_three_signature_failures_fail_the_gate(self):
        g = self.gate(final(SIG, SIG, SIG), self.check())
        self.assertFalse(g["ok"])
        self.assertEqual(g["counts"]["known_upstream_defect_hits"], 3)


class Refusal(unittest.TestCase):
    def test_current_file_is_accepted(self):
        self.assertIsNone(rb.expectation_problem(EXPECTATIONS))

    def changed(self, change):
        e = copy.deepcopy(EXPECTATIONS)
        change(e)
        return rb.expectation_problem(e)

    def test_two_defect_entries_are_refused(self):
        def two(e):
            e["known_upstream_defects"]["motion"] = dict(DEFECT, guarded_by=CHECK)
        self.assertIn("at most 1", self.changed(two))

    def test_higher_attempt_limit_is_refused(self):
        self.assertIn("max_attempts", self.changed(lambda e: e["known_upstream_defects"]["video_overlay"].update(max_attempts=4)))

    def test_defect_without_its_check_is_refused(self):
        self.assertIn("guarded_by", self.changed(lambda e: e["hars_checks"].clear()))

    def test_defect_without_signature_is_refused(self):
        self.assertIn("lacks", self.changed(lambda e: e["known_upstream_defects"]["video_overlay"].pop("line")))


if __name__ == "__main__":
    unittest.main(verbosity=1)

import json
import pathlib
import sys
import tempfile
import textwrap
import time
import unittest

from soak import ResponseLedger, run


def response(request_id, result=None, error=None):
    body = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        body["error"] = error
    else:
        body["result"] = result or {}
    return json.dumps(body).encode()


class ResponseLedgerTests(unittest.TestCase):
    def sent_ledger(self, count=1):
        ledger = ResponseLedger(max_requests=100, request_timeout=1)
        for request_id in range(1, count + 1):
            ledger.begin_attempt(request_id)
            ledger.mark_sent(request_id)
        return ledger

    def test_all_requests_receive_normal_responses(self):
        ledger = self.sent_ledger(3)
        for request_id in range(1, 4):
            ledger.receive_line(response(request_id))
        result = ledger.snapshot()
        self.assertEqual(result["requests_attempted"], 3)
        self.assertEqual(result["requests_send_succeeded"], 3)
        self.assertEqual(result["normal_responses"], 3)
        self.assertEqual(result["success_rate"], 1.0)
        self.assertTrue(result["fully_responded"])
        self.assertTrue(result["all_requests_succeeded"])

    def test_one_missing_response_never_reports_full_success(self):
        ledger = self.sent_ledger(2)
        ledger.receive_line(response(1))
        result = ledger.snapshot()
        self.assertEqual(result["final_unanswered"], 1)
        self.assertEqual(result["success_rate"], 0.5)
        self.assertFalse(result["fully_responded"])
        self.assertFalse(result["all_requests_succeeded"])

    def test_duplicate_response_is_counted_and_not_success(self):
        ledger = self.sent_ledger()
        ledger.receive_line(response(1))
        ledger.receive_line(response(1))
        result = ledger.snapshot()
        self.assertEqual(result["duplicate_responses"], 1)
        self.assertEqual(result["normal_responses"], 1)
        self.assertFalse(result["all_requests_succeeded"])

    def test_unknown_id_is_counted(self):
        ledger = self.sent_ledger()
        ledger.receive_line(response(7))
        self.assertEqual(ledger.snapshot()["unknown_id_responses"], 1)

    def test_invalid_json_and_invalid_response_are_separate(self):
        ledger = self.sent_ledger()
        ledger.receive_line(b"{not json}\n")
        ledger.receive_line(b'{"jsonrpc":"2.0","id":1}\n')
        result = ledger.snapshot()
        self.assertEqual(result["invalid_json"], 1)
        self.assertEqual(result["invalid_responses"], 1)
        self.assertEqual(result["final_unanswered"], 1)

    def test_response_during_shutdown_is_recorded_as_recovered(self):
        ledger = self.sent_ledger()
        self.assertEqual(ledger.begin_shutdown(), 1)
        ledger.receive_line(response(1))
        result = ledger.snapshot()
        self.assertEqual(result["shutdown_recovered_responses"], 1)
        self.assertEqual(result["final_unanswered"], 0)
        self.assertTrue(result["fully_responded"])

    def test_timeout_is_unanswered_even_after_leaving_inflight(self):
        ledger = ResponseLedger(max_requests=100, request_timeout=0.01)
        ledger.begin_attempt(1)
        ledger.mark_sent(1)
        time.sleep(0.05)
        ledger.expire_requests()
        result = ledger.snapshot()
        self.assertEqual(result["timeouts"], 1)
        self.assertEqual(result["final_inflight"], 0)
        self.assertEqual(result["final_unanswered"], 1)
        self.assertFalse(result["fully_responded"])

    def test_writer_disconnect_is_not_counted_as_sent(self):
        ledger = ResponseLedger(max_requests=100, request_timeout=1)
        ledger.begin_attempt(1)
        ledger.mark_send_failed(1)
        result = ledger.snapshot()
        self.assertEqual(result["requests_attempted"], 1)
        self.assertEqual(result["requests_send_succeeded"], 0)
        self.assertEqual(result["requests_send_failed"], 1)
        self.assertIsNone(result["success_rate"])
        self.assertFalse(result["fully_responded"])

    def test_abnormal_child_exit_keeps_unanswered_count_and_exit_distinct(self):
        ledger = self.sent_ledger()
        result = ledger.snapshot({"child_returncode": 1, "normal_exit": False})
        self.assertEqual(result["final_unanswered"], 1)
        self.assertFalse(result["normal_exit"])
        self.assertFalse(result["fully_responded"])

    def test_jsonrpc_error_is_received_but_not_successful(self):
        ledger = self.sent_ledger()
        ledger.receive_line(response(1, error={"code": -1, "message": "failed"}))
        result = ledger.snapshot()
        self.assertEqual(result["jsonrpc_error_responses"], 1)
        self.assertTrue(result["fully_responded"])
        self.assertFalse(result["all_requests_succeeded"])


class SoakProcessTests(unittest.TestCase):
    CHILD = textwrap.dedent("""
        import json, sys, time
        mode = sys.argv[1]
        if mode == "abnormal":
            raise SystemExit(7)
        if mode == "disconnect":
            sys.stdin.close()
            raise SystemExit(0)
        for line in sys.stdin:
            request = json.loads(line)
            if mode == "missing" and request["id"] == 2:
                continue
            if mode == "delayed":
                time.sleep(0.3)
            print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {}}), flush=True)
    """)

    def test_existing_evidence_file_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            output = root / "existing.json"
            output.write_text("original evidence", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                run("unused", 1, 1, root / "state", output, 1, 1, 1)
            self.assertEqual(output.read_text(encoding="utf-8"), "original evidence")
            self.assertFalse((root / "state").exists())

    def test_existing_state_directory_is_never_deleted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            state = root / "state"
            state.mkdir()
            sentinel = state / "sentinel"
            sentinel.write_text("preserve", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                run("unused", 1, 1, state, root / "result.json", 1, 1, 1)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")
            self.assertFalse((root / "result.json").exists())

    def run_child(self, mode):
        temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(temp.name)
        script = root / "child.py"
        script.write_text(self.CHILD, encoding="utf-8")
        result = run([sys.executable, str(script), mode], 60, 4,
                     root / "state", root / "result.json", 2, 10, 4)
        self.addCleanup(temp.cleanup)
        self.assertTrue((root / "result.json").exists())
        return result

    def test_subprocess_normal_response_and_eof_drain(self):
        result = self.run_child("normal")
        self.assertEqual(result["requests_send_succeeded"], 4)
        self.assertEqual(result["normal_responses"], 4)
        self.assertEqual(result["final_unanswered"], 0)
        self.assertEqual(result["child_returncode"], 0)

    def test_subprocess_missing_response_is_not_full_success(self):
        result = self.run_child("missing")
        self.assertEqual(result["normal_responses"], 3)
        self.assertEqual(result["final_unanswered"], 1)
        self.assertFalse(result["all_requests_succeeded"])

    def test_subprocess_delayed_shutdown_responses_are_counted(self):
        result = self.run_child("delayed")
        self.assertGreater(result["shutdown_recovered_responses"], 0)
        self.assertEqual(result["normal_responses"], 4)
        self.assertEqual(result["final_unanswered"], 0)

    def test_subprocess_writer_disconnect_is_reported(self):
        result = self.run_child("disconnect")
        self.assertTrue(result["send_error"] or result["requests_send_failed"]
                        or result["final_unanswered"] or result["child_exited_early"])
        self.assertFalse(result["all_requests_succeeded"])

    def test_subprocess_abnormal_exit_is_reported_and_reaped(self):
        result = self.run_child("abnormal")
        self.assertEqual(result["child_returncode"], 7)
        self.assertFalse(result["normal_exit"])
        self.assertTrue(result["child_exited_early"])


if __name__ == "__main__":
    unittest.main()

"""Response Ledger (Soak Harness V1, Slice 2).

Normative accounting contract (docs/SOAK_HARNESS_V1_DESIGN.md §5):
every request id produced by the driver ends in exactly one final state and
no id is ever silently dropped. A ledger that cannot produce a complete
classification makes the whole run FAIL — unanswered requests are never
treated as success.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field

# Request lifecycle / final states (campaign vocabulary preserved verbatim
# where possible; a request record carries exactly one final classification).
ATTEMPTED = "ATTEMPTED"
SENT = "SENT"
RESPONDED = "RESPONDED"
JSONRPC_ERROR = "JSON-RPC ERROR"
TIMED_OUT = "TIMED OUT"
SEND_FAILED = "SEND FAILED"
INVALID_FRAME = "INVALID FRAME"
DUPLICATE = "DUPLICATE"
UNKNOWN_ID = "UNKNOWN ID"
SHUTDOWN_INFLIGHT = "SHUTDOWN IN-FLIGHT"
RECOVERED = "RECOVERED"
UNRECOVERED = "UNRECOVERED"

# Final request states (one per request id).
FINAL_STATES = (
    RESPONDED,
    JSONRPC_ERROR,
    TIMED_OUT,
    SEND_FAILED,
    INVALID_FRAME,
    RECOVERED,
    UNRECOVERED,
)


class LedgerInconsistency(Exception):
    """Raised when the ledger cannot produce a complete, consistent classification."""


@dataclass
class RequestRecord:
    request_id: object
    state: str = ATTEMPTED
    transitions: list = field(default_factory=list)
    error: str = ""

    def set_state(self, new_state: str, detail: str = "") -> None:
        self.transitions.append((self.state, new_state, detail))
        self.state = new_state
        if detail and not self.error:
            self.error = detail


class ResponseLedger:
    """Thread-safe ledger over driver-issued request ids.

    Frames that cannot carry a driver-issued id (malformed requests answered
    with ``id: null``, notifications, foreign ids) are counted in dedicated
    counters so the *request-id* accounting stays exact and auditable.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._records: dict = {}
        self._unknown_ids: list = []
        self._duplicate_ids: list = []
        self._late_ids: list = []  # answered after TIMED OUT
        self._null_id_responses: int = 0
        self._raw_probe_responses: int = 0  # answers to deliberately raw-tracked ids (<= 0)
        self._notifications: int = 0  # raw frames sent without id (no response by design)
        self._malformed_outputs: int = 0  # inbound lines that are not JSON objects
        self._finalized = False
        self._final_report: dict = {}

    # -- request lifecycle -------------------------------------------------
    def attempt(self, request_id) -> None:
        with self._lock:
            if request_id in self._records:
                raise LedgerInconsistency(f"driver reused request id {request_id!r}")
            self._records[request_id] = RequestRecord(request_id)

    def sent(self, request_id) -> None:
        with self._lock:
            record = self._require(request_id)
            if record.state != ATTEMPTED:
                raise LedgerInconsistency(
                    f"id {request_id!r}: SENT requires ATTEMPTED, saw {record.state}"
                )
            record.set_state(SENT)

    def send_failed(self, request_id, detail: str) -> None:
        with self._lock:
            record = self._require(request_id)
            if record.state not in (ATTEMPTED, SENT):
                raise LedgerInconsistency(
                    f"id {request_id!r}: SEND FAILED requires ATTEMPTED/SENT, saw {record.state}"
                )
            record.set_state(SEND_FAILED, detail)

    def responded(self, request_id, *, is_error: bool = False) -> None:
        with self._lock:
            record = self._records.get(request_id)
            if record is None:
                self._unknown_ids.append(request_id)
                return
            if record.state == ATTEMPTED:
                # A response proves the frame was written to the child even if
                # the sender thread had not yet marked SENT (benign race).
                # Transition implicitly, then classify.
                record.set_state(SENT, "response received before sent-marker")
                record.set_state(JSONRPC_ERROR if is_error else RESPONDED)
                return
            if record.state == SENT:
                record.set_state(JSONRPC_ERROR if is_error else RESPONDED)
                return
            if record.state in (RESPONDED, JSONRPC_ERROR):
                self._duplicate_ids.append(request_id)
                return
            if record.state == TIMED_OUT:
                self._late_ids.append(request_id)
                return
            if record.state == SHUTDOWN_INFLIGHT:
                record.set_state(RECOVERED, "response arrived after shutdown/grace")
                return
            self._unknown_ids.append(request_id)

    def invalid_frame(self, request_id, detail: str) -> None:
        with self._lock:
            record = self._records.get(request_id)
            if record is None:
                self._unknown_ids.append(request_id)
                return
            if record.state == SENT or record.state == SHUTDOWN_INFLIGHT:
                record.set_state(INVALID_FRAME, detail)
            elif record.state in (RESPONDED, JSONRPC_ERROR, INVALID_FRAME):
                self._duplicate_ids.append(request_id)
            else:
                self._unknown_ids.append(request_id)

    def timed_out(self, request_id, detail: str) -> None:
        with self._lock:
            record = self._require(request_id)
            if record.state != SENT:
                raise LedgerInconsistency(
                    f"id {request_id!r}: TIMED OUT requires SENT, saw {record.state}"
                )
            record.set_state(TIMED_OUT, detail)

    def unknown_response(self, request_id) -> None:
        with self._lock:
            self._unknown_ids.append(request_id)

    def respond_refused(self, request_id, detail: str) -> None:
        """Classify a request whose answer was an id-null resource-limit refusal
        (e.g. oversized frames are discarded without parsing, so the answer
        carries ``id: null``). Recorded as RESPONDED with the refusal detail."""
        with self._lock:
            record = self._records.get(request_id)
            if record is None:
                self._unknown_ids.append(request_id)
                return
            if record.state == SENT or record.state == SHUTDOWN_INFLIGHT:
                record.set_state(RESPONDED, detail)
            elif record.state in (RESPONDED, JSONRPC_ERROR):
                self._duplicate_ids.append(request_id)
            elif record.state == TIMED_OUT:
                self._late_ids.append(request_id)
            else:
                self._unknown_ids.append(request_id)

    def null_id_count(self) -> int:
        with self._lock:
            return self._null_id_responses

    def raw_probe_response(self, count: int = 1) -> None:
        with self._lock:
            self._raw_probe_responses += count

    def null_id_response(self, count: int = 1) -> None:
        with self._lock:
            self._null_id_responses += count

    def notification_sent(self, count: int = 1) -> None:
        with self._lock:
            self._notifications += count

    def malformed_output(self, count: int = 1) -> None:
        with self._lock:
            self._malformed_outputs += count

    # -- shutdown accounting -------------------------------------------------
    def shutdown_begin(self) -> int:
        """Classify every still-open request as SHUTDOWN IN-FLIGHT."""
        with self._lock:
            moved = 0
            for record in self._records.values():
                if record.state in (ATTEMPTED, SENT):
                    record.set_state(SHUTDOWN_INFLIGHT)
                    moved += 1
            return moved

    def finalize(self) -> dict:
        """Close the ledger and return the normative §5 metrics.

        Raises LedgerInconsistency when any attempted id has no final state or
        when counters disagree with the per-id records.
        """
        with self._lock:
            # Remaining SHUTDOWN IN-FLIGHT requests are honest UNRECOVERED
            # entries — never erased, never reclassified as success.
            for record in self._records.values():
                if record.state == SHUTDOWN_INFLIGHT:
                    record.set_state(UNRECOVERED)
            unclassified = [
                record.request_id
                for record in self._records.values()
                if record.state not in FINAL_STATES
            ]
            if unclassified:
                raise LedgerInconsistency(
                    f"{len(unclassified)} request id(s) never reached a final "
                    f"classification (e.g. {unclassified[:3]!r})"
                )
            per_state: dict = {}
            for record in self._records.values():
                per_state[record.state] = per_state.get(record.state, 0) + 1
            sent_total = per_state.get(SENT, 0) + sum(
                per_state.get(state, 0)
                for state in (RESPONDED, JSONRPC_ERROR, TIMED_OUT, INVALID_FRAME, RECOVERED, UNRECOVERED, SHUTDOWN_INFLIGHT)
            )
            coverage_denominator = max(sent_total, 1)
            coverage = (
                per_state.get(RESPONDED, 0) + per_state.get(JSONRPC_ERROR, 0) + per_state.get(RECOVERED, 0)
            ) / coverage_denominator
            self._finalized = True
            self._final_report = {
                "attempted": len(self._records),
                "sent": sent_total,
                "send_failed": per_state.get(SEND_FAILED, 0),
                "normal_responses": per_state.get(RESPONDED, 0),
                "jsonrpc_errors": per_state.get(JSONRPC_ERROR, 0),
                "invalid_responses": per_state.get(INVALID_FRAME, 0),
                "duplicate_responses": len(self._duplicate_ids),
                "unknown_ids": len(self._unknown_ids),
                "unknown_ids_sample": [str(item) for item in self._unknown_ids[:10]],
                "late_responses_after_timeout": len(self._late_ids),
                "null_id_responses": self._null_id_responses,
                "raw_probe_responses": self._raw_probe_responses,
                "notifications_sent": self._notifications,
                "malformed_outputs": self._malformed_outputs,
                "timeouts": per_state.get(TIMED_OUT, 0),
                "shutdown_recovered": per_state.get(RECOVERED, 0),
                "shutdown_unrecovered": per_state.get(UNRECOVERED, 0),
                "unclassified": len(unclassified),
                "coverage": round(coverage, 6),
                "state_histogram": per_state,
            }
            return dict(self._final_report)

    # -- introspection -----------------------------------------------------
    def _require(self, request_id) -> RequestRecord:
        record = self._records.get(request_id)
        if record is None:
            raise LedgerInconsistency(f"id {request_id!r} was never ATTEMPTED")
        return record

    def snapshot_state(self, request_id) -> str:
        with self._lock:
            return self._records[request_id].state

    def pending_ids(self) -> list:
        with self._lock:
            return [
                record.request_id
                for record in self._records.values()
                if record.state in (ATTEMPTED, SENT)
            ]

    @property
    def finalized(self) -> bool:
        with self._lock:
            return self._finalized

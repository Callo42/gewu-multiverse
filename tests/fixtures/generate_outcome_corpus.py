#!/usr/bin/env python3
"""Build `native-outcome-corpus.json` from the failure-signal inventory.

The inventory is the external input named by the Worker Order. This script copies
each probe's exact stdout/stderr lines into one checked-in fixture and adds the
outcome/reason the inventory's assessment calls correct. The Rust test
`agent_run::native::corpus_tests` replays every entry through the product parser.

Workspace-private paths and harness session/thread ids are replaced with neutral
placeholders while the lines are copied, so the public fixture does not carry the
private host or task. The inventory's SHA-256 still records the exact input.

Run from the repository root:

    python3 tests/fixtures/generate_outcome_corpus.py \
        /path/to/input/inventory

The generated file is deterministic: entries are sorted by probe id, placeholders
are assigned in sorted id order, and the inventory's SHA-256 is recorded.
"""

import hashlib
import json
import os
import re
import sys

# The inventory ran under a private workspace. Keep the shape of the paths (the
# parser sees a cwd, a workspace and harness files) without the private root. The
# root is found structurally — the prefix before the inventory's `harness/` or
# `probes/` directory — so this public script holds no private host or task path.
PRIVATE_WORKSPACE = re.compile(r'/home/[^\s"\']*?/(?=(?:harness|probes)/)')
PLACEHOLDER_ROOT = "/tmp/fixture"
SESSION_ID = re.compile(
    r"session-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
)
ANY_ID = re.compile(
    r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"
)


def sanitize_entries(entries):
    """Replace private paths and session/thread ids in every copied line.

    The mapping is global and sorted, so regenerating the fixture from the same
    inventory always writes the same bytes. Only the raw stream lines change; the
    recorded expectations and the inventory SHA-256 do not.
    """
    lines = [
        line
        for entry in entries
        for name in ("stdout", "stderr")
        for line in entry[name]
    ]
    session_ids = sorted(
        {match.group(1) for line in lines for match in SESSION_ID.finditer(line)}
    )
    session_map = {
        uuid: f"session-{index:04d}" for index, uuid in enumerate(session_ids)
    }
    other_ids = sorted(
        {
            match.group(0)
            for line in lines
            for match in ANY_ID.finditer(line)
            if match.group(0) not in session_map
        }
    )
    stream_map = {uuid: f"stream-{index:04d}" for index, uuid in enumerate(other_ids)}

    def clean(line):
        line = PRIVATE_WORKSPACE.sub(PLACEHOLDER_ROOT + "/", line)
        line = SESSION_ID.sub(lambda match: session_map[match.group(1)], line)
        return ANY_ID.sub(
            lambda match: stream_map.get(match.group(0), match.group(0)), line
        )

    for entry in entries:
        entry["stdout"] = [clean(line) for line in entry["stdout"]]
        entry["stderr"] = [clean(line) for line in entry["stderr"]]


def load_inventory(root):
    with open(os.path.join(root, "inventory.json"), "rb") as handle:
        raw = handle.read()
    return json.loads(raw), hashlib.sha256(raw).hexdigest()


def evidence_lines(root, entry, name):
    path = os.path.join(root, "evidence", entry["probe_id"], name)
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8", errors="replace") as handle:
        return [line.rstrip("\n") for line in handle]


def expected(entry):
    """The inventory's `assessment.correct`, made machine-checkable.

    The parser is judged on the harness's terminal outcome and the reason it can
    truthfully carry. A failed tool is a warning on top of a successful terminal
    turn, so it is asserted through `tool_failure_contains` rather than by
    pretending the session failed.
    """
    harness = entry["harness"]
    kind = entry["failure_kind"]
    probe = entry["probe_id"]
    out = {
        "disposition": None,
        "reason_contains": [],
        "text": None,
        "text_absent": False,
        "tool_failure_contains": None,
        "saw_tool": None,
        "supervisor": "natural",
    }

    def failed(*fragments):
        out["disposition"] = "failed"
        out["reason_contains"] = list(fragments)
        return out

    def succeeded(text="probe ok"):
        out["disposition"] = "succeeded"
        out["text"] = text
        return out

    if kind.startswith("cancellation_"):
        out["disposition"] = "stopped"
        out["supervisor"] = "stopped"
        return out
    if kind == "control_ok":
        return succeeded()
    if kind == "model_not_configured":
        if harness == "pi":
            # The product always writes the selected model id into models.json, so
            # this probe (an unknown model id) is not reachable through the product.
            return succeeded()
        return failed("model")
    if kind == "recovered_after_provider_error":
        return succeeded()
    if kind == "recovered_after_stream_truncation":
        if harness == "opencode":
            # OpenCode's own exit is 1; text after a failed attempt is not a
            # terminal success.
            out["disposition"] = "failed"
            out["reason_contains"] = ["stream"]
            out["text_absent"] = True
            return out
        return succeeded()
    if kind == "tool_nonzero_exit":
        succeeded()
        if probe == "codex-tool-false":
            # The default Codex sandbox refused the command without any item
            # event. There is no supported surface to name the denial.
            out["saw_tool"] = False
        else:
            out["tool_failure_contains"] = ["exit code 1"]
        return out
    if kind == "sandbox_refusal":
        succeeded()
        if probe == "codex-sandbox-write":
            out["saw_tool"] = False
        elif harness == "dsh":
            out["tool_failure_contains"] = [
                "permission denied",
                "sandbox",
                "exit code 1",
            ]
        else:
            out["tool_failure_contains"] = ["permission denied", "exit code 1"]
        return out
    if kind == "tool_signal_termination":
        succeeded()
        out["tool_failure_contains"] = ["SIGTERM"]
        return out
    if kind == "provider_http_400":
        return failed("400")
    if kind == "provider_http_401":
        return failed("AUTH" if harness == "dsh" else "401")
    if kind == "provider_http_404":
        return failed("404")
    if kind == "provider_http_429":
        return failed("RATE_LIMIT" if harness == "dsh" else "429")
    if kind == "provider_http_500":
        if harness == "dsh":
            return failed("SERVER", "500")
        if harness == "codex":
            # Codex 0.164.0 emits only the generic "high demand" prose; it never
            # exposes the 500 status, so the product cannot preserve it.
            return failed("high demand")
        return failed("500")
    if kind == "connection_refused":
        if harness == "opencode":
            return failed("ECONNREFUSED")
        if probe == "codex-connrefused":
            out["disposition"] = "failed"
            out["supervisor"] = "deadline"
            out["reason_contains"] = ["connection failed"]
            return out
        return failed("connection")
    if kind == "connection_reset":
        if harness == "codex":
            return failed("error sending request")
        if harness == "dsh":
            return failed("connection error")
        return failed("connection" if harness == "pi" else "reset")
    if kind == "stream_truncated":
        return failed("stream")
    if kind == "stream_malformed":
        if harness == "opencode":
            return failed("invalid")
        if harness == "codex":
            # The harness hides the malformed JSON behind its generic
            # "stream closed before response.completed" message.
            return failed("stream")
        return failed("malformed")
    if kind == "request_timeout":
        if probe == "codex-timeout-shape":
            out["disposition"] = "timed_out"
            out["supervisor"] = "deadline"
            out["reason_contains"] = ["time limit"]
            return out
        if harness == "dsh":
            return failed("TIMEOUT")
        if harness == "opencode":
            return failed("timed out")
        if harness == "pi":
            return failed("timed out")
        return failed("timeout")
    if kind == "harness_internal_config_error":
        if harness == "codex":
            return failed("config.toml", "invalid key-value")
        if harness == "dsh":
            return failed("overlay", "failed to parse")
        if harness == "opencode":
            return failed("model unavailable")
        return failed("unknown provider")
    raise SystemExit(f"no expectation for {probe} ({harness}/{kind})")


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inventory_root = sys.argv[1]
    inventory, digest = load_inventory(inventory_root)
    entries = []
    for entry in sorted(inventory["entries"], key=lambda item: item["probe_id"]):
        check = expected(entry)
        exit_info = entry.get("exit", {})
        entries.append(
            {
                "probe_id": entry["probe_id"],
                "harness": entry["harness"],
                "failure_kind": entry["failure_kind"],
                "assessment": entry["assessment"]["correct"],
                "exit_code": exit_info.get("code"),
                "stdout": evidence_lines(inventory_root, entry, "stdout.jsonl"),
                "stderr": evidence_lines(inventory_root, entry, "stderr.txt"),
                "expected": check,
            }
        )
    sanitize_entries(entries)
    corpus = {
        "source": "Callo42/gewu-multiverse failure-signal inventory",
        "inventory_sha256": digest,
        "product_commit": inventory["product"]["commit"],
        "entries": entries,
    }
    here = os.path.dirname(os.path.abspath(__file__))
    target = os.path.join(here, "native-outcome-corpus.json")
    with open(target, "w", encoding="utf-8") as handle:
        json.dump(corpus, handle, separators=(",", ":"), sort_keys=True)
        handle.write("\n")
    print(f"wrote {target}: {len(entries)} entries, inventory sha256 {digest}")


if __name__ == "__main__":
    main()

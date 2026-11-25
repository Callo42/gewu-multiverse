"""Linux headless integration fixture. Run as a regular user with GEWU_FORGEJO_BINARY."""

import http.server
from concurrent.futures import ThreadPoolExecutor
import ctypes
import contextlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request


@contextlib.contextmanager
def unrelated_service():
    with subprocess.Popen([sys.executable, __file__, "fixture-service"],
                          stdout=subprocess.PIPE, text=True) as service:
        try:
            assert service.stdout.readline().strip() == "ready"
            yield service
            assert service.poll() is None, "normal finish affected an independent service"
        finally:
            service.terminate()


def main(mode):
    os.umask(0o077)
    binary = Path(sys.argv[1]).resolve(strict=True)
    received = threading.Event()
    completed = threading.Event()
    slow_seconds = 20
    cleanup_case = mode in ("cleanup", "cleanup-late", "interrupted")
    handover_case = mode == "handover"
    allow_finish = threading.Event()
    other_agent_called = threading.Event()
    handover_lock = threading.Lock()
    handover_state = {
        "calls": 0,
        "run2_at": None,
        "saw_refreshed": False,
    }
    handover_run1_started = threading.Event()
    handover_release_run1 = threading.Event()

    class Model(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            request = self.rfile.read(int(self.headers["Content-Length"]))
            if b"Independent agent fixture" in request:
                other_agent_called.set()
            content = {"role": "assistant", "content": "No assigned work."}
            reason = "stop"
            if handover_case:
                with handover_lock:
                    handover_state["calls"] += 1
                    call_no = handover_state["calls"]
                if call_no == 1:
                    handover_run1_started.set()
                    handover_release_run1.wait(15)
                    content = {"role": "assistant", "content": "No assigned issues found; nothing to do."}
                elif call_no == 2:
                    with handover_lock:
                        handover_state["run2_at"] = time.monotonic()
                        handover_state["saw_refreshed"] = b"HANDOVER-WORK-777" in request
                    content = {"role": "assistant", "content": None, "tool_calls": [{
                        "id": "handover_comment", "type": "function", "function": {
                            "name": "forgejo_comment", "arguments": json.dumps({
                                "repository": "team/workspace",
                                "number": 1,
                                "body": "Processed handover task in post-wake run."
                            })}}]}
                    reason = "tool_calls"
                elif call_no == 3:
                    content = {"role": "assistant", "content": "Processed handover task."}
                    reason = "stop"
                else:
                    content = {"role": "assistant", "content": "No assigned issues found; nothing to do."}
            elif cleanup_case and not received.is_set():
                received.set()
                content = {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "fixture", "type": "function", "function": {
                        "name": "shell", "arguments": json.dumps({"command": "printf finished"})}}]}
                reason = "tool_calls"
            elif cleanup_case:
                completed.set()
                allow_finish.wait(10)
            elif not received.is_set():
                received.set()
                time.sleep(slow_seconds)
                completed.set()
            payload = json.dumps({"choices": [{"message": content, "finish_reason": reason}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            try:
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass  # Expected if the agent was stopped during its request.

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="mv-control-") as tmp:
            root = Path(tmp)
            base = [str(binary), "--config-dir", str(root / "cfg"),
                    "--state-dir", str(root / "state"), "--data-dir", str(root / "data")]

            def cli(*args):
                return subprocess.run(base + list(args), capture_output=True, text=True, timeout=35)

            inst = root / "instruction"
            inst.write_text("Report no work.\n")
            key = root / "test-key"
            key.write_text("loopback-fixture\n")
            cfg = cli("--configure", "--accept-terms", "--agents", "2", "--agent", "agent-1",
                      "--program", "builtin", "--base-url", f"http://127.0.0.1:{httpd.server_port}/v1",
                      "--model", "local-fixture", "--api-key-file", str(key),
                      "--instruction-file", str(inst), "--stop-all")
            assert cfg.returncode == 0, cfg.stderr
            second_inst = root / "second-instruction"
            second_inst.write_text("Independent agent fixture. Report no work.\n")
            cfg = cli("--configure", "--agent", "agent-2", "--program", "builtin",
                      "--instruction-file", str(second_inst))
            assert cfg.returncode == 0, cfg.stderr
            with (root / "daemon.log").open("w") as log:
                def launch_daemon():
                    return subprocess.Popen(base + ["--headless", "--allow-idle"], stdin=subprocess.DEVNULL,
                                            stdout=log, stderr=log, start_new_session=True)

                daemon = launch_daemon()
                try:
                    deadline = time.monotonic() + 100
                    while time.monotonic() < deadline:
                        status = cli("--status", "--json")
                        if status.returncode == 0 and json.loads(status.stdout).get("healthy"):
                            break
                        if daemon.poll() is not None:
                            raise RuntimeError(f"daemon exited {daemon.returncode} before readiness")
                        time.sleep(0.2)
                    else:
                        raise TimeoutError("headless readiness")

                    def control(action, name):
                        started = time.monotonic()
                        result = cli("--control", action, name, "--json")
                        elapsed = time.monotonic() - started
                        record = {"command": action, "agent": name, "elapsed_ms": round(elapsed*1000, 1),
                                  "exit": result.returncode, "model_finished": completed.is_set()}
                        if result.returncode == 0:
                            reply = json.loads(result.stdout)
                            status = reply.get("status", reply)
                            record["desired"] = {a["name"]: a["desired"] for a in status["agents"]}
                            expected = "started" if action == "--start-agent" else "stopped"
                            assert record["desired"][name] == expected, record
                        print(json.dumps(record), flush=True)
                        return result

                    if handover_case:
                        # Create a real Forgejo issue #1 so the post-wake run performs real remote work.
                        ready_status = json.loads(cli("--status", "--json").stdout)
                        forgejo_url = ready_status["forgejo"]["url"].rstrip("/")
                        creds_text = (root / "cfg" / "credentials.toml").read_text()
                        token_section = creds_text.split("[forgejo_agent_tokens]", 1)[1]
                        token_match = re.search(r'^"?agent-1"?\s*=\s*"([^"]+)"', token_section, re.MULTILINE)
                        assert token_match, "missing agent-1 Forgejo token in credentials.toml"
                        agent_token = token_match.group(1)

                        def forgejo_api(method, api_path, body=None):
                            data = json.dumps(body).encode() if body is not None else None
                            req = urllib.request.Request(
                                f"{forgejo_url}{api_path}",
                                data=data,
                                method=method,
                                headers={
                                    "Authorization": f"token {agent_token}",
                                    "Content-Type": "application/json",
                                },
                            )
                            with urllib.request.urlopen(req, timeout=15) as resp:
                                return json.loads(resp.read().decode())

                        # Wake run #1 with --stop-on-no-work and pause it mid-flight.
                        wake1 = cli("--control", "--start-agent", "agent-1", "--stop-on-no-work", "--json")
                        assert wake1.returncode == 0, wake1.stderr
                        assert handover_run1_started.wait(15), "run #1 did not start in handover scenario"
                        in_flight_status = json.loads(cli("--status", "--json").stdout)
                        agent1 = next(a for a in in_flight_status["agents"] if a["name"] == "agent-1")
                        assert agent1["desired"] == "started", agent1
                        assert agent1["runs_total"] == 1, agent1
                        assert agent1["session"].startswith("running"), agent1
                        assert agent1["stop_on_no_work"] is True, agent1

                        # While run #1 is still Running, create the handover issue, refresh the
                        # instruction, and send a fresh --control --start-agent --stop-on-no-work wake.
                        forgejo_api("POST", "/api/v1/repos/team/workspace/issues", {
                            "title": "Handover task #1",
                            "body": "Assigned while previous session was still running.",
                        })
                        (root / "cfg" / "agents" / "agent-1" / "instruction.md").write_text(
                            "HANDOVER-WORK-777: handle newly assigned work.\n"
                        )
                        wake2 = cli("--control", "--start-agent", "agent-1", "--stop-on-no-work", "--json")
                        assert wake2.returncode == 0, wake2.stderr
                        wake2_json = json.loads(wake2.stdout)
                        wake2_agent = next(a for a in wake2_json["status"]["agents"] if a["name"] == "agent-1")
                        assert wake2_agent["desired"] == "started" and wake2_agent["stop_on_no_work"] is True

                        # Release run #1 so it finishes with NoWork; agent-1 must NOT auto-park
                        # prematurely and must launch run #2 immediately without waiting NO_WORK_DELAY (20s).
                        released_at = time.monotonic()
                        handover_release_run1.set()

                        park_deadline = time.monotonic() + 35
                        final_status = None
                        while time.monotonic() < park_deadline:
                            assert daemon.poll() is None, f"daemon exited {daemon.returncode} during handover"
                            st = json.loads(cli("--status", "--json").stdout)
                            a1 = next(a for a in st["agents"] if a["name"] == "agent-1")
                            if a1["desired"] == "stopped":
                                if a1["runs_total"] < 2:
                                    raise AssertionError(
                                        f"premature auto-park during task handover: {a1}"
                                    )
                                final_status = st
                                break
                            time.sleep(0.15)
                        else:
                            raise TimeoutError("timed out waiting for post-wake handover runs and auto-park")

                        a1_final = next(a for a in final_status["agents"] if a["name"] == "agent-1")
                        assert a1_final["runs_total"] == 3, a1_final
                        assert a1_final["session"] == "idle", a1_final
                        assert a1_final["stop_on_no_work"] is False, a1_final
                        assert final_status["idle_standby"] is True, final_status
                        assert final_status["healthy"] is True, final_status
                        with handover_lock:
                            run2_at = handover_state["run2_at"]
                            saw_refreshed = handover_state["saw_refreshed"]
                            calls_after_park = handover_state["calls"]
                        assert run2_at is not None, "post-wake run #2 never invoked the model"
                        handover_latency_s = run2_at - released_at
                        assert handover_latency_s < 10.0, (
                            f"post-wake run #2 waited too long ({handover_latency_s:.2f}s >= 10s; "
                            "expected immediate wake instead of 20s NO_WORK_DELAY)"
                        )
                        assert saw_refreshed, "post-wake run #2 did not receive refreshed instruction"
                        comments = forgejo_api("GET", "/api/v1/repos/team/workspace/issues/1/comments")
                        assert any("Processed handover task in post-wake run." in c.get("body", "") for c in comments), comments
                        time.sleep(3)
                        with handover_lock:
                            assert handover_state["calls"] == calls_after_park == 4, handover_state
                        print(json.dumps({
                            "handover_premature_park": False,
                            "handover_latency_ms": round(handover_latency_s * 1000, 1),
                            "handover_saw_refreshed_instruction": saw_refreshed,
                            "runs_total": a1_final["runs_total"],
                            "model_calls": calls_after_park,
                            "parked_idle": True,
                        }), flush=True)
                        return

                    assert control("--start-agent", "agent-1").returncode == 0
                    assert received.wait(15), "model did not receive the slow request"
                    if cleanup_case:
                        assert completed.wait(15), "shell tool did not reach the final response"
                        cap, = (root / "state").rglob("shell.capability")
                        pid = int((cap.parent / "session.pid").read_text())
                        with unrelated_service() if mode == "cleanup-late" else contextlib.nullcontext():
                            if mode == "interrupted":
                                pidfd = os.pidfd_open(pid)
                                try:
                                    argv = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                                    assert b"--run-agent" in argv and os.fsencode(cap.parent) in argv
                                    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                                finally:
                                    os.close(pidfd)
                            allow_finish.set()
                            deadline = time.monotonic() + 10
                            while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
                                time.sleep(0.005)
                            assert not Path(f"/proc/{pid}").exists(), "first supervisor did not exit"
                            assert control("--start-agent", "agent-2").returncode == 0
                            status = json.loads(cli("--status", "--json").stdout)
                            if mode == "interrupted":
                                retry = control("--start-agent", "agent-1")
                                assert retry.returncode != 0 and "cleanup" in retry.stderr.lower(), retry.stderr
                                assert cap.exists(), "unverified recovery discarded its evidence"
                                assert other_agent_called.wait(15), "unaffected agent did not run"
                                assert control("--stop-agent", "agent-2").returncode == 0
                                daemon.send_signal(signal.SIGTERM)
                                assert daemon.wait(timeout=75) == 0
                                other_agent_called.clear()
                                daemon = launch_daemon()
                                deadline = time.monotonic() + 90
                                while time.monotonic() < deadline:
                                    assert daemon.poll() is None, "one blocked agent prevented daemon restart"
                                    state = cli("--status", "--json")
                                    view = json.loads(state.stdout) if state.stdout.strip() else {}
                                    if (view.get("daemon_pid") == daemon.pid
                                            and view.get("forgejo", {}).get("state") == "running"
                                            and all(a["provisioning"] == "ready" for a in view["agents"])):
                                        break
                                    time.sleep(0.15)
                                else:
                                    raise TimeoutError("daemon startup alongside an unverified run")
                                assert control("--start-agent", "agent-2").returncode == 0
                                assert other_agent_called.wait(15), "other agent did not run after reopen"
                                assert cap.exists(), "restart discarded the first agent's evidence"
                                print(json.dumps({"uncertain_run_blocked": True,
                                                  "other_agent_available_after_reopen": True}), flush=True)
                                return
                            assert status["healthy"], status["blocking_reasons"]
                            assert control("--stop-agent", "agent-1").returncode == 0
                            before = {p.parent for p in (root / "state").rglob("plan.json")
                                      if p.parent.parent == cap.parent.parent}
                            assert control("--start-agent", "agent-1").returncode == 0
                            deadline = time.monotonic() + 15
                            while time.monotonic() < deadline:
                                if any(p.parent not in before for p in cap.parent.parent.glob("*/plan.json")):
                                    break
                                time.sleep(0.05)
                            else:
                                raise AssertionError("new run for the original agent did not start")
                            print(json.dumps({"healthy": True, "original_agent_restarted": True,
                                              "service_fixture": mode}), flush=True)
                        return
                    print(json.dumps({"slow_request_received": True, "model_delay_s": slow_seconds}), flush=True)
                    if mode == "during-stop":
                        with ThreadPoolExecutor(max_workers=1) as executor:
                            stop = executor.submit(control, "--stop-agent", "agent-1")
                            assert control("--start-agent", "agent-2").returncode == 0
                            assert stop.result().returncode == 0
                    else:
                        assert control("--start-agent", "agent-2").returncode == 0
                    assert control("--stop-agent", "agent-2").returncode == 0
                    assert not completed.is_set(), "control finished after the slow reply"
                    assert completed.wait(slow_seconds + 2)
                    time.sleep(1)
                    print(json.dumps({"slow_request_completed": True}), flush=True)
                finally:
                    if daemon.poll() is None:
                        daemon.send_signal(signal.SIGTERM)
                    daemon.wait(timeout=75)
                    print(json.dumps({"daemon_exit": daemon.returncode}), flush=True)
    finally:
        httpd.shutdown()
        httpd.server_close()


if __name__ == "__main__":
    if sys.argv[1:] == ["fixture-service"]:
        # Simulate the ordinary service's restriction on reading its descriptors.
        assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0
        print("ready", flush=True)
        signal.pause()
    elif sys.argv[2:] in (["cleanup"], ["interrupted"]):
        with unrelated_service():
            main(sys.argv[2])
    else:
        main(sys.argv[2] if len(sys.argv) > 2 else "slow")

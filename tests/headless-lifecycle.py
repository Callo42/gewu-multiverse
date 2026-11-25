"""Linux headless integration fixture. Run as a regular user with GEWU_FORGEJO_BINARY."""

import http.server
from concurrent.futures import ThreadPoolExecutor
import ctypes
import contextlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time


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
    allow_finish = threading.Event()
    other_agent_called = threading.Event()

    class Model(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            request = self.rfile.read(int(self.headers["Content-Length"]))
            if b"Independent agent fixture" in request:
                other_agent_called.set()
            content = {"role": "assistant", "content": "No assigned work."}
            reason = "stop"
            if cleanup_case and not received.is_set():
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

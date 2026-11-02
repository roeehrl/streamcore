#!/usr/bin/env python3
"""Exercise CLI workload forwarding through real pinned QUIC and TLS sockets."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parents[2]
binary = root / "target/debug/examples" / ("scheduling_probe.exe" if os.name == "nt" else "scheduling_probe")
with tempfile.TemporaryDirectory(prefix="streamcore-scheduling-cli-") as temporary:
    directory = Path(temporary)
    processes = []
    with (directory / "processes.log").open("w+") as log:
        try:
            client = subprocess.Popen([str(binary), "client", temporary, "127.0.0.1", "2", "128", "262144"],
                stdout=log, stderr=subprocess.STDOUT)
            processes.append(client)
            deadline = time.monotonic() + 10
            while not (directory / "client-public.json").exists():
                if client.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Client failed to publish its test certificate")
                time.sleep(.05)
            server = subprocess.Popen([str(binary), "server", temporary], stdout=log, stderr=subprocess.STDOUT)
            processes.append(server)
            if client.wait(timeout=30) != 0:
                log.seek(0)
                raise RuntimeError(log.read()[-2000:])
            reports = json.loads((directory / "report.json").read_text())
            assert [report["transport"] for report in reports] == ["quic", "tls_tcp"]
            for report in reports:
                bulk = report["lanes"][4]
                assert bulk["offered"]["bytes_per_message"] == 262144
                assert bulk["offered"]["rate_per_second"] == 128
                assert bulk["offered"]["scheduled"] == 256
                for lane in report["lanes"]:
                    offered = lane["offered"]
                    assert lane["received"] > 0 and lane["received"] == offered["accepted"]
                    assert offered["write_completion"]["samples"] == lane["received"]
                    assert lane["round_trip"]["samples"] == lane["received"]
                    assert lane["receive_gap"]["samples"] == lane["received"] - 1
                    assert offered["scheduled"] == offered["accepted"] + offered["missed_deadlines"] + offered["backpressured"]
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
print("CLI workload forwarding and delivery accounting pass on QUIC and TLS/TCP")

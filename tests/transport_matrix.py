#!/usr/bin/env python3
"""Exercise the real Rust binary over both MCP transports and configured stores."""
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

BINARY = str(Path(os.environ.get("ROSETTA_BINARY", "target/debug/rosetta")).resolve())
POSTGRES = os.environ.get("ROSETTA_TEST_POSTGRES_URL")
REQUESTS = [
    ("search_nodes", {"query": "helper"}),
    ("grep_code", {"pattern": "fixture-marker"}),
    ("query_graph", {"pattern": "callers_of", "target": "fixture/core.py::Function::helper"}),
    ("get_impact_radius", {"changed_files": ["fixture/core.py"]}),
    ("get_review_context", {"changed_files": ["fixture/core.py"]}),
    ("list_graph_stats", {}),
    ("grep_code", {"pattern": "[", "regex": True}),
    ("search_nodes", {"query": "x", "limit": 0}),
]
INIT = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {},
    "clientInfo": {"name": "rosetta-contract-test", "version": "1"},
}}


def lines(stream, target):
    for line in iter(stream.readline, ""):
        target.put(line)
    target.put(None)


def read_json(target, expected):
    while True:
        line = target.get(timeout=15)
        assert line is not None, "stdio closed unexpectedly"
        value = json.loads(line)  # Any stdout diagnostics fail here.
        if value.get("id") == expected:
            return value


def result_payload(response):
    result = response["result"]
    return {"isError": result.get("isError", False),
            "value": json.loads(result["content"][0]["text"])}


def stdio(args):
    process = subprocess.Popen([BINARY, *args, "serve", "--transport", "stdio"],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True)
    output, errors = queue.Queue(), queue.Queue()
    threading.Thread(target=lines, args=(process.stdout, output), daemon=True).start()
    threading.Thread(target=lines, args=(process.stderr, errors), daemon=True).start()
    def send(value):
        process.stdin.write(json.dumps(value) + "\n")
        process.stdin.flush()
    try:
        send(INIT)
        assert read_json(output, 1)["result"]["serverInfo"]["name"] == "rosetta"
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        names = [t["name"] for t in read_json(output, 2)["result"]["tools"]]
        assert set(names) == {name for name, _ in REQUESTS}
        results = []
        for request_id, (name, arguments) in enumerate(REQUESTS, 3):
            send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                  "params": {"name": name, "arguments": arguments}})
            results.append(result_payload(read_json(output, request_id)))
        if Path("/usr/sbin/lsof").exists():
            sockets = subprocess.run(["/usr/sbin/lsof", "-a", "-p", str(process.pid),
                                      "-iTCP", "-sTCP:LISTEN"], capture_output=True, text=True)
            assert not sockets.stdout.strip(), "stdio process opened a TCP listener"
        process.stdin.close()
        assert process.wait(timeout=10) == 0, "stdio failed to shut down on EOF"
        while not output.empty():
            line = output.get()
            if line is not None:
                json.loads(line)
        return results
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        process.stdout.close()
        process.stderr.close()


def http(args):
    process = subprocess.Popen([BINARY, *args, "serve", "--transport", "http", "--listen", "127.0.0.1:0"],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    errors = queue.Queue()
    threading.Thread(target=lines, args=(process.stderr, errors), daemon=True).start()
    try:
        while True:
            line = errors.get(timeout=15)
            assert line is not None, "HTTP server exited before listening"
            if line.startswith("Rosetta MCP: "):
                endpoint = line.strip().split("Rosetta MCP: ", 1)[1]
                break
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        def send(value):
            request = urllib.request.Request(endpoint, data=json.dumps(value).encode(), headers=headers)
            with urllib.request.urlopen(request, timeout=15) as response:
                if response.headers.get("Mcp-Session-Id"):
                    headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
                body = response.read().decode()
                if not body:
                    return None
                if "text/event-stream" in response.headers.get("Content-Type", ""):
                    events = [json.loads(line[5:].strip()) for line in body.splitlines() if line.startswith("data:") and line[5:].strip()]
                    return next(event for event in events if event.get("id") == value.get("id"))
                return json.loads(body)
        assert send(INIT)["result"]["serverInfo"]["name"] == "rosetta"
        headers["MCP-Protocol-Version"] = "2025-06-18"
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        listing = send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        assert len(listing["result"]["tools"]) == 6
        results = []
        for request_id, (name, arguments) in enumerate(REQUESTS, 3):
            results.append(result_payload(send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                                                "params": {"name": name, "arguments": arguments}})))
        for extra in [{"Origin": "https://attacker.invalid"}, {"Host": "attacker.invalid"}, {"Host": "attacker@localhost"}]:
            request = urllib.request.Request(endpoint, data=json.dumps(INIT).encode(), headers={**headers, **extra})
            try:
                urllib.request.urlopen(request, timeout=5)
                raise AssertionError("unsafe Host/Origin accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 403
        native = endpoint.removesuffix("/mcp") + "/api/v1/tools/list_graph_stats"
        with urllib.request.urlopen(urllib.request.Request(native, data=b"{}", headers={"Content-Type": "application/json"}), timeout=5) as response:
            assert json.load(response) == results[5]["value"]
        try:
            urllib.request.urlopen(urllib.request.Request(native, data=b"{}", headers={"Content-Type": "application/json", "Origin": "https://attacker.invalid"}), timeout=5)
            raise AssertionError("native API bypassed Origin validation")
        except urllib.error.HTTPError as error:
            assert error.code == 403
        for path in ["/", "/search", "/browse/fixture", "/api/repos"]:
            try:
                urllib.request.urlopen(endpoint.removesuffix("/mcp") + path, timeout=5)
                raise AssertionError("excluded UI/compatibility route exists: " + path)
            except urllib.error.HTTPError as error:
                assert error.code == 404
        process.send_signal(signal.SIGTERM)
        assert process.wait(timeout=10) == 0
        assert not process.stdout.read(), "HTTP startup wrote to stdout"
        return results
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        process.stdout.close()
        process.stderr.close()


def main():
    baseline = None
    with tempfile.TemporaryDirectory(prefix="rosetta-transports-") as temporary:
        root = Path(temporary)
        fixture = root / "fixture"
        fixture.mkdir()
        (fixture / "core.py").write_text("def helper():\n    return 'fixture-marker'\n")
        (fixture / "app.py").write_text("from core import helper\ndef run():\n    return helper()\n")
        (fixture / ".env").write_text("SECRET=hidden-fixture\n")
        backends = [("sqlite", str(root / "graph.db"))]
        if POSTGRES:
            backends.append(("postgres", POSTGRES))
        for backend, database in backends:
            args = ["--storage", backend, "--database", database]
            subprocess.run([BINARY, *args, "index", "--path", str(fixture), "--name", "fixture"], check=True, capture_output=True)
            for transport in [stdio, http]:
                actual = transport(args)
                assert actual[-1]["isError"] and actual[-2]["isError"]
                if baseline is None:
                    baseline = actual
                else:
                    assert actual == baseline, f"result mismatch: {backend}/{transport.__name__}"
                print(f"PASS {backend}/{transport.__name__}: six tools, errors, protocol and shutdown")
            subprocess.run([BINARY, *args, "remove", "fixture"], check=True, capture_output=True)
    if not POSTGRES:
        print("PostgreSQL matrix not run; set ROSETTA_TEST_POSTGRES_URL to an isolated test database.")


if __name__ == "__main__":
    main()

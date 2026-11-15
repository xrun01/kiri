#!/usr/bin/env python3
"""What Testcontainers (docker-java, .NET, Go) needs of the engine API, asked
the way those clients ask: raw HTTP/1.1 on one kept-alive connection.

    tests/testcontainers-api.py SOCKET [version|cmd|keepalive|follow|follow-created|timestamps ...]

Prints one line per check, "ok <name>" or "FAIL <name>: why", and exits 1 if
any failed. The containers it makes (tc-api-*) are left running.
"""
import io
import json
import re
import socket
import sys
import tarfile
import time

SOCK = sys.argv[1]
IMAGE = "alpine:latest"


class Conn:
    """One HTTP/1.1 connection, kept for every request, as a client pool keeps it."""

    def __init__(self, timeout=30):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.settimeout(timeout)
        self.s.connect(SOCK)
        self.buf = b""

    def _fill(self):
        d = self.s.recv(65536)
        if not d:
            raise EOFError("the engine closed the connection")
        self.buf += d

    def line(self):
        while b"\r\n" not in self.buf:
            self._fill()
        l, self.buf = self.buf.split(b"\r\n", 1)
        return l

    def take(self, n):
        while len(self.buf) < n:
            self._fill()
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, method, path, body=None, ctype="application/json"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        head = f"{method} /v1.32{path} HTTP/1.1\r\nHost: localhost:2375\r\nAccept: application/json\r\n"
        if body is not None:
            head += f"Content-Type: {ctype}\r\nContent-Length: {len(body)}\r\n"
        self.s.sendall(head.encode() + b"\r\n" + (body or b""))

    def head(self):
        status = int(self.line().split()[1])
        headers = {}
        while True:
            l = self.line()
            if not l:
                return status, headers
            k, v = l.decode().split(":", 1)
            headers[k.strip().lower()] = v.strip()

    def chunks(self):
        """The chunks of a chunked body, as they arrive."""
        while True:
            n = int(self.line().split(b";")[0], 16)
            if n == 0:
                self.line()
                return
            data = self.take(n)
            self.take(2)
            yield data

    def request(self, method, path, body=None, ctype="application/json"):
        self.send(method, path, body, ctype)
        status, headers = self.head()
        if headers.get("transfer-encoding") == "chunked":
            data = b"".join(self.chunks())
        else:
            data = self.take(int(headers.get("content-length", "0"))) if status not in (204, 304) else b""
        return status, headers, data


def frames(chunks):
    """Docker's multiplexed stream: [stream, 0, 0, 0, len u32 BE] data."""
    body = b""
    for c in chunks:
        body += c
        while len(body) >= 8:
            if body[0] not in (0, 1, 2) or body[1:4] != b"\0\0\0":
                raise ValueError(f"bad frame header {body[:8]!r}")
            n = int.from_bytes(body[4:8], "big")
            if len(body) < 8 + n:
                break
            yield body[0], body[8:8 + n]
            body = body[8 + n:]


def fresh(c, name, cmd, **extra):
    c.request("DELETE", f"/containers/{name}?force=1")
    status, _, data = c.request("POST", f"/containers/create?name={name}", {"Image": IMAGE, "Cmd": cmd, **extra})
    assert status == 201, f"create {status} {data[:200]!r}"
    return json.loads(data)["Id"]


def check_version():
    c = Conn()
    _, _, v = c.request("GET", "/version")
    v = json.loads(v)
    major = int(v["Version"].split(".")[0])
    # Testcontainers refuses an engine below 1.6.0; Kiri's own 0.x read as one.
    assert major >= 20, f"Version {v['Version']} is not a Docker Engine version"
    assert v.get("KiriVersion"), "no KiriVersion"
    assert any(x["Name"] == "Kiri" for x in v["Components"]), "no Kiri component"
    _, _, i = c.request("GET", "/info")
    assert json.loads(i)["ServerVersion"] == v["Version"], "info's ServerVersion differs"
    return f"engine {v['Version']}, Kiri {v['KiriVersion']}"


def check_cmd():
    # Testcontainers sends "Cmd": [] when no command is set (ryuk, redis…):
    # Docker runs the image's CMD (#121).
    c = Conn()
    fresh(c, "tc-api-cmd", [])
    _, _, d = c.request("GET", "/containers/tc-api-cmd/json")
    cfg = json.loads(d)["Config"]
    assert cfg["Cmd"] == ["/bin/sh"], f"Cmd {cfg['Cmd']}"
    status, _, data = c.request("POST", "/containers/tc-api-cmd/start")
    assert status == 204, f"start {status} {data[:200]!r}"
    return "Cmd [] runs the image's CMD"


def check_keepalive():
    # Testcontainers copies files into a created container, then starts it on
    # the same pooled connection (MySQLContainer's my.cnf).
    c = Conn()
    fresh(c, "tc-api-cp", ["sleep", "600"])
    tar = io.BytesIO()
    with tarfile.open(fileobj=tar, mode="w") as t:
        data = b"[mysqld]\n"
        info = tarfile.TarInfo("my.cnf")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
    status, headers, _ = c.request("PUT", "/containers/tc-api-cp/archive?path=/tmp", tar.getvalue(), "application/x-tar")
    assert status == 200, f"archive {status}"
    closing = headers.get("connection", "").lower() == "close"
    try:
        status, _, data = c.request("POST", "/containers/tc-api-cp/start")
    except (EOFError, BrokenPipeError, ConnectionResetError) as e:
        raise AssertionError(f"start on the same connection: {e}" + ("" if closing else " (no Connection: close was sent)"))
    assert status == 204, f"start {status} {data[:200]!r}"
    return "start after a copy on one connection"


def wait_lines(c, cid, pattern, times, timeout):
    """LogMessageWaitStrategy: follow=true since=0, count frames matching."""
    rx = re.compile(r"(?s).*" + pattern + r".*\s")
    c.s.settimeout(timeout)
    c.send("GET", f"/containers/{cid}/logs?since=0&stdout=true&stderr=true&follow=true")
    status, headers = c.head()
    assert status == 200, f"logs {status}"
    assert headers.get("content-type") == "application/vnd.docker.multiplexed-stream", headers.get("content-type")
    seen = 0
    t0 = time.time()
    try:
        for _, data in frames(c.chunks()):
            seen += bool(rx.fullmatch(data.decode("utf-8", "replace")))
            if seen >= times:
                return time.time() - t0
    except socket.timeout:
        pass
    raise AssertionError(f"{seen} of {times} matching lines")


SCRIPT = "echo starting; sleep 2; echo 'ready for connections'; sleep 2; echo 'ready for connections' >&2; sleep 600"


def check_follow():
    c = Conn()
    cid = fresh(c, "tc-api-logs", ["sh", "-c", SCRIPT])
    status, _, _ = c.request("POST", f"/containers/{cid}/start")
    assert status == 204, f"start {status}"
    took = wait_lines(c, cid, "ready for connections", 2, 30)
    return f"2 lines seen through follow in {took:.1f} s"


def check_follow_created():
    # A follow opened before the container starts sees what it prints after.
    c = Conn()
    cid = fresh(c, "tc-api-logs2", ["sh", "-c", SCRIPT])
    starter = Conn()
    import threading
    threading.Timer(0.5, lambda: starter.request("POST", f"/containers/{cid}/start")).start()
    took = wait_lines(c, cid, "ready for connections", 2, 30)
    return f"2 lines seen through a follow opened while created, in {took:.1f} s"


def check_timestamps():
    c = Conn()
    cid = fresh(c, "tc-api-ts", ["echo", "hi"])
    c.request("POST", f"/containers/{cid}/start")
    c.request("POST", f"/containers/{cid}/wait")
    _, _, data = c.request("GET", f"/containers/{cid}/logs?stdout=1&timestamps=1")
    lines = [d for _, d in frames([data])]
    assert len(lines) == 1, lines
    # One RFC 3339 stamp with nanoseconds, then the line.
    assert re.fullmatch(rb"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{9}Z hi\n", lines[0]), lines[0]
    return "one RFC 3339 stamp"


CHECKS = {
    "version": check_version, "cmd": check_cmd, "keepalive": check_keepalive,
    "follow": check_follow, "follow-created": check_follow_created, "timestamps": check_timestamps,
}

if __name__ == "__main__":
    c = Conn(timeout=300)
    status, _, _ = c.request("POST", "/images/create?fromImage=alpine&tag=latest")
    failed = 0
    for name in sys.argv[2:] or CHECKS:
        try:
            print(f"ok {name}: {CHECKS[name]()}", flush=True)
        except Exception as e:  # noqa: BLE001 — every failure is a line
            failed += 1
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
    sys.exit(1 if failed else 0)

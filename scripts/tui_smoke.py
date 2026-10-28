#!/usr/bin/env python3
"""TUI smoke test in a real pseudo-terminal.

Starts `diskdrift` (bare, so TTY detection must open the TUI), navigates a few
screens, quits, and checks the terminal was restored: the alternate screen was
left and the cursor shown again. Uses --config/--db inside target/test-tmp.

usage: scripts/tui_smoke.py [path/to/diskdrift]
"""
import fcntl, os, pty, re, select, signal, struct, sys, termios, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/debug/diskdrift")
TMP = os.path.join(ROOT, "target/test-tmp/tui-smoke")
os.makedirs(TMP, exist_ok=True)
cfg = os.path.join(TMP, "config.json")
with open(cfg, "w") as f:
    f.write('{"schemaVersion":1}')


def run(keys, settle=0.6, cancel_with_signal=False):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        os.environ["TERM"] = "xterm-256color"
        os.execv(BIN, [BIN, "--config", cfg, "--db", os.path.join(TMP, "store")])
    out = b""

    def pump(t):
        nonlocal out
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    out += os.read(fd, 65536)
                except OSError:
                    return

    pump(1.0)
    for k in keys:
        os.write(fd, k)
        pump(settle)
    if cancel_with_signal:
        os.kill(pid, signal.SIGTERM)
    deadline = time.time() + 10
    status = None
    while time.time() < deadline:
        p, st = os.waitpid(pid, os.WNOHANG)
        if p:
            status = st
            break
        pump(0.1)
    pump(0.2)
    if status is None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        raise SystemExit("FAIL: TUI did not exit")
    return out, status


def check(name, out, status, expect_exit=0):
    text = out.decode("utf-8", "replace")
    assert b"\x1b[?1049h" in out, f"{name}: never entered alternate screen"
    assert out.rfind(b"\x1b[?1049l") > out.rfind(b"\x1b[?1049h"), f"{name}: alternate screen not left"
    assert out.rfind(b"\x1b[?25h") > out.rfind(b"\x1b[?25l"), f"{name}: cursor not restored"
    code = os.waitstatus_to_exitcode(status)
    assert code == expect_exit, f"{name}: exit code {code}"
    print(f"ok  {name}")
    return text


text = check("navigate and quit", *run([b"?", b"\x1b", b"u", b"\x1b", b"\x1b[B", b"\x1b[C", b"q"]))
plain = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", " ", text)
assert "Scan" in plain and "shortcuts" in plain and "Usage" in plain, "expected menu, help and usage to render"
check("ctrl-c quits", *run([b"\x03"]))
check("quit while the doctor runs", *run([b"i", b"q"], settle=0.8))
# A real scan through the UI: type the root, confirm, wait, look at history, quit.
tree = os.path.join(TMP, "tree")
os.makedirs(os.path.join(tree, "sub"), exist_ok=True)
for i in range(50):
    with open(os.path.join(tree, "sub", f"f{i}.txt"), "w") as f:
        f.write("x" * (i * 100))
store = os.path.join(TMP, "store")
if os.path.isdir(store):
    import shutil
    shutil.rmtree(store)
keys = [b"s"] + [b"\x7f"] * 40 + [tree.encode(), b"\t", b"tui-scan", b"\r", b"\r"]
out, status = run(keys + [b"", b"h", b"t", b"\t", b"d", b"q"], settle=0.8)
check("scan from the TUI", out, status)
import json, glob
metas = [json.load(open(p)) for p in glob.glob(os.path.join(store, "snapshots", "*.json"))]
assert any(m["name"] == "tui-scan" and m["status"] == "COMPLETED" for m in metas), metas
print("ok  snapshot written by the TUI is COMPLETED")
# Cancel a running scan: a large read-only tree, Esc + y after a moment.
keys = [b"s"] + [b"\x7f"] * 120 + [b"/Applications", b"\t", b"tui-cancel", b"\r", b"\r"]
out, status = run(keys + [b"", b"\x1b", b"y", b"", b"q"], settle=0.7)
check("cancel a running scan", out, status)
metas = [json.load(open(p)) for p in glob.glob(os.path.join(store, "snapshots", "*.json"))]
m = [m for m in metas if m["name"] == "tui-cancel"]
assert m and m[0]["status"] in ("CANCELLED", "COMPLETED", "PARTIAL"), metas
assert not glob.glob(os.path.join(store, "snapshots", "*.tmp")), "no temporary data left"
print(f"ok  cancelled scan recorded as {m[0]['status']}")
print("TUI smoke test passed")

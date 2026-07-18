#!/usr/bin/env python3
"""PTY integration tests for logtui: drive the real binary in a pty and
assert on the rendered screen (via pyte)."""
import fcntl, os, pty, select, struct, subprocess, sys, termios, time
import pyte

BIN = "/tmp/logtui/target/release/logtui"
COLS, ROWS = 100, 30

class TuiSession:
    def __init__(self, shell_cmd):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

        def preexec():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        env = dict(os.environ, TERM="xterm-256color")
        self.proc = subprocess.Popen(["bash", "-c", shell_cmd],
            stdin=slave, stdout=slave, stderr=slave, env=env,
            close_fds=True, preexec_fn=preexec)
        os.close(slave)

    def pump(self, seconds=0.3):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.05)
            if r:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.stream.feed(data)

    def send(self, s):
        os.write(self.master, s.encode())

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def quit_and_wait(self):
        try:
            self.send("q")
        except OSError:
            pass
        self.pump(0.3)
        try:
            return self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return -9

    def close(self):
        try:
            os.close(self.master)
        except OSError:
            pass
        if self.proc.poll() is None:
            self.proc.kill()

def check(name, cond, screen_text):
    print(f"[{'PASS' if cond else 'FAIL'}] {name}")
    if not cond:
        print("----- screen -----\n" + screen_text + "\n------------------")
    return cond

def main():
    ok = True

    # 1. Finite input: read to EOF, line count, clean quit.
    s = TuiSession(f"seq 1 50 | {BIN}")
    s.pump(0.6)
    t = s.text()
    ok &= check("finite: shows EOF status", "EOF" in t, t)
    ok &= check("finite: 50 lines counted", "50 lines" in t, t)
    ok &= check("finite: follow snapped to bottom", "50" in t, t)
    rc = s.quit_and_wait()
    ok &= check("finite: clean exit on q", rc == 0, f"rc={rc}")
    s.close()

    # 2. Streaming: LIVE while producing, EOF when done.
    s = TuiSession(f"(for i in $(seq 1 5); do echo stream-$i; sleep 0.4; done) | {BIN}")
    s.pump(0.5)
    t = s.text()
    ok &= check("stream: LIVE while producing", "LIVE" in t, t)
    ok &= check("stream: early lines rendered", "stream-1" in t, t)
    s.pump(2.5)
    t = s.text()
    ok &= check("stream: EOF after producer done", "EOF" in t, t)
    ok &= check("stream: all 5 lines arrived", "5 lines" in t, t)
    rc = s.quit_and_wait()
    ok &= check("stream: clean exit", rc == 0, f"rc={rc}")
    s.close()

    # 3. Search: / opens bar, live filter, n navigation, Esc clears.
    s = TuiSession("printf '%s\\n' 'error one' 'ok' 'error two' 'fine' 'error three' | " + BIN)
    s.pump(0.5)
    s.send("/")
    s.pump(0.2)
    ok &= check("search: bar visible after /", "[fuzzy]" in s.text(), s.text())
    s.send("error")
    s.pump(0.4)
    t = s.text()
    ok &= check("search: 3 matches live", "3 matches" in t, t)
    ok &= check("search: non-match hidden", "fine" not in t, t)
    s.send("\r")
    s.pump(0.2)
    s.send("n")
    s.pump(0.2)
    s.send("\x1b")
    s.pump(0.3)
    t = s.text()
    ok &= check("search: Esc clears filter", "5 lines" in t, t)
    ok &= check("search: hidden line back", "fine" in t, t)
    rc = s.quit_and_wait()
    ok &= check("search: clean exit", rc == 0, f"rc={rc}")
    s.close()

    # 4. --no-follow: stays at top while streaming; G resumes follow.
    s = TuiSession(f"(for i in $(seq 1 100); do echo row-$i; sleep 0.02; done) | {BIN} --no-follow")
    s.pump(1.2)
    t = s.text()
    first = t.split("\n")[0] if "\n" in t else t
    ok &= check("no-follow: stays at top", "row-1" in first, t)
    ok &= check("no-follow: no FOLLOW indicator", "FOLLOW" not in t, t)
    s.send("G")
    s.pump(0.3)
    ok &= check("no-follow: G resumes follow", "FOLLOW" in s.text(), s.text())
    rc = s.quit_and_wait()
    ok &= check("no-follow: clean exit", rc == 0, f"rc={rc}")
    s.close()

    # 5. --regex mode.
    s = TuiSession("printf '%s\\n' 'err code 42' 'err code x' 'nothing' | " + BIN + " --regex")
    s.pump(0.5)
    s.send("/")
    s.send(r"err code [0-9]+")
    s.pump(0.4)
    t = s.text()
    ok &= check("regex: [regex] mode shown", "[regex]" in t, t)
    ok &= check("regex: 1 match for pattern", "1 matches" in t, t)
    ok &= check("regex: non-digit excluded", "err code x" not in t, t)
    s.send("\x1b")
    s.pump(0.2)
    rc = s.quit_and_wait()
    ok &= check("regex: clean exit", rc == 0, f"rc={rc}")
    s.close()

    # 6. High throughput: 200k lines, buffer caps at 10k, UI stays alive.
    s = TuiSession(f"seq 1 200000 | {BIN}")
    s.pump(3.0)
    t = s.text()
    ok &= check("throughput: capped at 10,000 lines", "10,000 lines" in t, t)
    ok &= check("throughput: dropped count reported", "dropped" in t, t)
    rc = s.quit_and_wait()
    ok &= check("throughput: clean exit under load", rc == 0, f"rc={rc}")
    s.close()

    print()
    print("ALL PASS" if ok else "SOME FAILURES")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()

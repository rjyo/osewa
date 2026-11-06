#!/usr/bin/env python3
"""Link outage tests: every tool, same host, same outage.

    OSEWA_CONFIG_DIR=... bench/outage.py <osewa> <host> [rounds] [tools]

<osewa> is the path to the client, <host> the ssh and osewa name of a
throwaway host: this cuts its network, so it needs root there, and tc and
iptables. tools is a comma list of osewa, mosh, et and ssh; each must already
work against the host. The outage lasts OUT seconds.

The host drops the client's traffic both ways for N seconds. Ingress goes
through iptables; egress through netem loss, because an iptables OUTPUT drop
returns EPERM to the sender, which a real outage never does.

  resume:  a counter ticks in the session. How long after the link is back
           does the first new byte arrive?
  backlog: a command writes MB of output during the outage, then a marker.
           How long after the link is back does the marker arrive?

Times use the host's clock for the end of the outage, corrected by an offset
measured over ssh, so they are good to a few tens of ms.
"""
import fcntl, json, os, pty, select, signal, struct, subprocess, sys, termios, time

if len(sys.argv) < 3:
    raise SystemExit(__doc__)
OSEWA, HOST = sys.argv[1], sys.argv[2]
RUNS = int(sys.argv[3]) if len(sys.argv) > 3 else 2
TOOLS = sys.argv[4].split(",") if len(sys.argv) > 4 else ["osewa", "mosh", "et", "ssh"]
OUT = 15
# Telemetry off: a benchmark shouldn't report to anyone.
ENV = dict(os.environ, ET_NO_TELEMETRY="1", LANG="en_US.UTF-8", LC_ALL="en_US.UTF-8")


def ssh(cmd, timeout=30):
    return subprocess.run(["ssh", "-o", "ControlPath=none", HOST, cmd], capture_output=True, text=True, timeout=timeout, env=ENV).stdout.strip()


def offset():
    """host clock minus ours, from the best of a few round trips."""
    best = None
    for _ in range(5):
        a = time.time()
        h = float(ssh("date +%s.%N"))
        b = time.time()
        if best is None or b - a < best[0]:
            best = (b - a, h - (a + b) / 2)
    return best[1]


def outage(seconds, ip):
    script = (
        f"iptables -I INPUT -s {ip} -j DROP; tc qdisc add dev eth0 root netem loss 100%; "
        f"sleep {seconds}; tc qdisc del dev eth0 root; iptables -D INPUT -s {ip} -j DROP; date +%s.%N > /tmp/back"
    )
    ssh(f"rm -f /tmp/back; nohup sh -c '{script}' >/dev/null 2>&1 &")


class Pty:
    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe(argv[0], argv, ENV)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.bytes = 0

    def pump(self, seconds, want=None, after=None):
        """Reads for seconds. Returns the time of the first read at or after
        `after`, or of the first read holding `want`, else None."""
        end, tail = time.time() + seconds, b""
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.02)
            if not r:
                continue
            try:
                b = os.read(self.fd, 1 << 16)
            except OSError:
                return None
            now = time.time()
            self.bytes += len(b)
            # The whole read: a redraw puts the marker mid-chunk, with the
            # prompt and row clears after it.
            seen = tail + b
            tail = seen[-64:]
            if want is not None and want in seen:
                return now
            if after is not None and now >= after:
                return now
        return None

    def send(self, s):
        os.write(self.fd, s.encode())

    def kill(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except OSError:
            pass
        # Master first: on macOS a dying process waits for its unread tty
        # output to drain, so waiting before closing never returns.
        os.close(self.fd)
        for _ in range(50):
            try:
                if os.waitpid(self.pid, os.WNOHANG)[0]:
                    return
            except OSError:
                return
            time.sleep(0.1)


def session(tool):
    if tool == "osewa":
        sid = subprocess.run([OSEWA, "new", HOST], capture_output=True, text=True, env=ENV).stdout.strip().split()[-1]
        return Pty([OSEWA, "attach", "--predict", "never", HOST, sid]), sid
    if tool == "mosh":
        return Pty(["mosh", "--predict=never", HOST]), None
    if tool == "et":
        return Pty(["et", HOST]), None
    return Pty(["ssh", "-tt", "-o", "ControlPath=none", HOST]), None


def clean(tool, p, sid):
    p.kill()
    if sid:
        subprocess.run([OSEWA, "kill", HOST, sid], capture_output=True, env=ENV)
    ssh("pkill etterminal; pkill mosh-server; pkill -f 'bench-tick'; true")


def back_time(off):
    for _ in range(40):
        v = ssh("cat /tmp/back 2>/dev/null")
        if v:
            return float(v) - off
        time.sleep(0.5)
    raise SystemExit("the host never reported the link back")


def resume(tool, seconds, ip, off):
    p, sid = session(tool)
    p.pump(4)
    p.send("i=0; while :; do i=$((i+1)); echo tick$i; sleep 0.1; done # bench-tick\r")
    p.pump(2)
    outage(seconds, ip)
    # Every read is stamped, and sorted out once the host says when the link
    # came back, so nothing here touches the network during the outage.
    t0 = time.time()
    stamps, end = [], t0 + seconds + 45
    while time.time() < end:
        if p.pump(0.25, after=0) is not None:
            stamps.append(time.time())
            # Back: the host's own stamp settles exactly when, below.
            if stamps[-1] > t0 + seconds + 1:
                break
    back = back_time(off)
    clean(tool, p, sid)
    late = [t for t in stamps if t >= back]
    return late[0] - back if late else None


def backlog(tool, seconds, mb, ip, off):
    p, sid = session(tool)
    p.pump(4)
    lines = mb * 1_000_000 // 8
    p.send(f"sleep 3; seq 1000000 {1000000 + lines}; echo BENCH''DONE\r")
    p.pump(1)
    outage(seconds, ip)
    t = p.pump(seconds + 120, want=b"BENCHDONE")
    back = back_time(off)
    got = p.bytes
    clean(tool, p, sid)
    return (None if t is None else t - back), got


def main():
    ip = ssh("echo ${SSH_CLIENT%% *}")
    off = offset()
    print(f"clock offset {off:+.3f} s", flush=True)
    out = {}
    for r in range(RUNS):
        for tool in TOOLS:
            d = resume(tool, OUT, ip, off)
            out.setdefault(f"resume {tool}", []).append(d)
            print(f"run {r+1} resume {tool}: {d}", flush=True)
            for mb in (20, 200):
                d, got = backlog(tool, OUT, mb, ip, off)
                out.setdefault(f"backlog{mb} {tool}", []).append(d)
                print(f"run {r+1} backlog{mb} {tool}: {d} ({got/1e6:.3f} MB read)", flush=True)
    print(json.dumps(out))


main()

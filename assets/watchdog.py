#!/usr/bin/env python3
"""Detached VM-side cost-guard watchdog (stdlib only).

Runs independently on the Colab VM so auto-stop deadlines and GPU/heartbeat
idle timeouts release the runtime via `POST http://$TBE_RUNTIME_ADDR/unassign`
even if Antigravity or the local MCP server closes.
"""

import argparse
import fcntl
import ipaddress
import json
import math
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

GPU_BUSY_PERCENT = 5.0
RELEASE_ATTEMPTS = 3


def runtime_url(addr):
    """Accept a literal Colab bridge IPv4 address and the runtime control port."""
    if not addr or not addr.isascii() or addr.count(":") != 1:
        return None
    host, port = addr.split(":")
    try:
        ip = ipaddress.IPv4Address(host)
    except ValueError:
        return None
    if ip.packed[0] != 172 or not 16 <= ip.packed[1] <= 31 or port != "8011":
        return None
    return f"http://{ip}:{port}/unassign"


def valid_config(cfg, instance):
    def positive(value):
        return type(value) in (int, float) and math.isfinite(value) and value > 0

    deadline = cfg.get("deadline_epoch")
    idle = cfg.get("idle_timeout_s")
    grace = cfg.get("grace_s")
    return (isinstance(instance, str) and re.fullmatch(r"[A-Za-z0-9-]+", instance)
            and cfg.get("instance") == instance and cfg.get("cancelled") is False
            and (deadline is None or positive(deadline)) and (idle is None or positive(idle))
            and (deadline is not None or idle is not None) and positive(cfg.get("started"))
            and type(grace) in (int, float) and math.isfinite(grace) and grace >= 0)


def read_config(path):
    with open(path) as fh:
        data = json.load(fh)
    return data if isinstance(data, dict) else {}


def announce(pid_path, instance, active):
    with open("/proc/self/stat") as stat:
        started = stat.read().rsplit(") ", 1)[1].split()[19]
    fd, temp = tempfile.mkstemp(dir=os.path.dirname(pid_path))
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump([os.getpid(), started, instance, active], fh)
        os.replace(temp, pid_path)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class Watchdog:
    def __init__(self, config_path, instance):
        self.config_path = config_path
        self.instance = instance
        self.release_url = runtime_url(os.environ.get("TBE_RUNTIME_ADDR", ""))
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.config = {}
        self.log_path = None
        self.last_gpu_busy = 0.0
        self.started = time.time()

    def log(self, msg):
        line = "%s %s\n" % (time.strftime("%Y-%m-%dT%H:%M:%S%z"), msg)
        try:
            with open(self.log_path or "/tmp/colab-mcp-watchdog.log", "a") as fh:
                fh.write(line)
        except OSError:
            pass

    def load(self):
        try:
            self.config = read_config(self.config_path)
        except (OSError, ValueError):
            pass
        self.log_path = self.config.get("log") or self.log_path
        started = self.config.get("started")
        if isinstance(started, (int, float)):
            self.started = float(started)

    def gpu_util(self):
        try:
            out = subprocess.run(
                ["nvidia-smi", "--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"],
                capture_output=True,
                text=True,
                timeout=15,
                check=True,
            ).stdout
            vals = [float(x.strip()) for x in out.splitlines() if x.strip()]
            return max(vals) if vals else None
        except Exception:
            return None

    def release(self, reason, still_due):
        if self.release_url is None:
            self.log("cannot release: invalid Colab runtime address")
            return False
        for attempt in range(1, RELEASE_ATTEMPTS + 1):
            self.load()
            if (self.config.get("instance") != self.instance or self.config.get("cancelled")
                    or not still_due()):
                return False
            try:
                req = urllib.request.Request(self.release_url, data=b"", method="POST")
                with self.http.open(req, timeout=30) as resp:
                    if resp.status == 200:
                        self.log("released runtime (%s)" % reason)
                        return True
                    self.log("unassign HTTP %d (attempt %d)" % (resp.status, attempt))
            except Exception as exc:
                self.log("unassign error %s (attempt %d)" % (type(exc).__name__, attempt))
            time.sleep(10)
        return False

    def idle_for(self, now):
        last = self.started
        hb = self.config.get("heartbeat")
        if hb:
            try:
                modified = os.path.getmtime(hb)
                if modified <= now:
                    last = max(last, modified)
            except OSError:
                pass
        util = self.gpu_util()
        if util is not None and util >= GPU_BUSY_PERCENT:
            self.last_gpu_busy = now
        return now - max(last, self.last_gpu_busy)

    def run(self):
        self.load()
        if not self.release_url or self.config.get("instance") != self.instance:
            return 1
        self.log("watchdog started pid=%d" % os.getpid())
        while True:
            self.load()
            if self.config.get("instance") != self.instance:
                # A new policy is provisional until the controller releases
                # its lock. Rollback can restore this instance in the meantime.
                lock_path = os.path.join(os.path.dirname(self.config_path), "watchdog.lock")
                with open(lock_path, "a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_SH)
                    self.load()
            if self.config.get("cancelled") or self.config.get("instance") != self.instance:
                self.log("cancelled; exiting without releasing")
                return 0
            now = time.time()
            dl = self.config.get("deadline_epoch")
            if isinstance(dl, (int, float)) and now >= dl:
                if self.release("deadline", lambda: self.config.get("deadline_epoch") == dl):
                    return 0
            idle = self.config.get("idle_timeout_s")
            grace = self.config.get("grace_s", 0)
            if (isinstance(idle, (int, float)) and idle > 0
                    and isinstance(grace, (int, float)) and now - self.started >= grace):
                idle_s = self.idle_for(now)
                # The GPU probe can block. Re-read the policy before unassigning.
                self.load()
                if (self.config.get("instance") == self.instance and not self.config.get("cancelled")
                        and self.config.get("idle_timeout_s") == idle and idle_s >= idle):
                    if self.release("idle %ds" % idle_s, lambda: self.config.get("idle_timeout_s") == idle):
                        return 0
            poll = self.config.get("poll_s", 15)
            time.sleep(max(5, min(60, poll if isinstance(poll, (int, float)) else 15)))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    watchdog = Watchdog(args.config, args.instance)
    if watchdog.release_url is None or not re.fullmatch(r"[A-Za-z0-9-]+", args.instance):
        return 1
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    base = os.path.join(os.path.dirname(os.path.abspath(args.config)), f"watchdog.{args.instance}")
    pid_path = base + ".pid"
    try:
        if not valid_config(read_config(base + ".candidate"), args.instance):
            return 1
        announce(pid_path, args.instance, False)
        until = time.monotonic() + 8
        while time.monotonic() < until:
            watchdog.load()
            if valid_config(watchdog.config, args.instance):
                announce(pid_path, args.instance, True)
                return watchdog.run()
            time.sleep(0.05)
        return 1
    except (OSError, ValueError):
        return 1
    finally:
        try:
            os.unlink(pid_path)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    sys.exit(main())

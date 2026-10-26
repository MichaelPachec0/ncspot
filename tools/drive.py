#!/usr/bin/env python3
"""Drive an isolated ncspot over IPC and check what its views show.

Each scenario starts ncspot (built with --features harness) in a private tmux server, with
its own base path (-b, which holds the cache and the config) and runtime directory (the IPC
socket), using a copy of the cached login. The user's own instance, cache and config are never
touched. Credentials, tokens, the username, Spotify IDs and library contents are never printed.

Usage: python3 tools/drive.py [--play] [--keep] [--no-build] [--settle-timeout S] [scenario ...]
Exit codes: 0 all PASS, 1 any FAIL, 2 setup error, 3 only BLOCKED (rate limited, retry later).
"""

import argparse
import glob
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REAL_CACHE = os.path.join(
    os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "ncspot"
)
REAL_CONFIG = os.path.join(
    os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "ncspot"
)
SECRET_WORDS = ("token", "bearer", "auth")
URI_RE = re.compile(r"(spotify[:/])([a-z]+)[:/][A-Za-z0-9]{22}")
STATUS_RE = re.compile(r"\[[^\]\n]*\]")
SOCKET_WAIT = 15.0
DUMP_WAIT = 15.0
QUIET_MS = 500
CACHE_FILES = {
    "Tracks": "tracks.db",
    "Albums": "albums.db",
    "Artists": "artists.db",
    "Podcasts": "shows.db",
    "Your Episodes": "episodes.db",
}
LIBRARY_TABS = list(CACHE_FILES)

TRACK = "4uLU6hMCjMI75M1A2tKUQC"
ARTIST = "0OdUWJ0sBjDrqHygGUXeCF"
SHOW = "4rOoJ6Egrf8K2IrywzwOMk"
REMOVED_EPISODE = "2rKMiQw2A58GL5ZHH8jfHo"


class Fail(Exception):
    """A scenario check failed."""


class Blocked(Exception):
    """A client the scenario needs is rate limited."""


class SetupError(Exception):
    """The harness itself could not start."""


# --- pure helpers (unit tested) ---


def should_write_back(copied, temp, real_now):
    """Whether a token refreshed in the temp instance should replace the cached one.

    True only when the temp copy changed, still holds a non-empty access_token, and
    the real file is still exactly what was copied (nobody else wrote it meanwhile)."""
    if temp is None or real_now is None or temp == copied or real_now != copied:
        return False
    try:
        return bool(json.loads(temp).get("access_token"))
    except (ValueError, AttributeError):
        return False


def write_private(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.chmod(path, 0o600)


def copy_token(src, dst):
    """Copy a token file and return the bytes copied: one read, so the write-back
    compares against exactly what the temp instance started with."""
    with open(src, "rb") as f:
        data = f.read()
    write_private(dst, data)
    return data


def write_atomic(path, data):
    """Replace path with data via a private part file, removed if anything fails."""
    part = path + ".tmp"
    try:
        write_private(part, data)
        os.replace(part, path)
    except BaseException:
        try:
            os.unlink(part)
        except OSError:
            pass
        raise


def settled(d):
    """Every tab idle, no call in flight, and quiet for at least QUIET_MS."""
    quiet = d.get("quiet_ms")
    return (
        all(not t["busy"] for t in d["tabs"])
        and d["in_flight"] == 0
        and quiet is not None
        and quiet >= QUIET_MS
    )


def redact(lines, username):
    out = []
    for line in lines:
        low = line.lower()
        secret = any(w in low for w in SECRET_WORDS)
        if secret or (username and username in line):
            out.append("[redacted]")
        else:
            out.append(URI_RE.sub(r"\1\2:[id]", line))
    return out


def status_segments(lines):
    """Only the bracketed status bar segments of a screen capture, such as
    `[rate limited: shared 24s]`. Rows, titles and the playing track show library contents."""
    segments = [seg for line in lines for seg in STATUS_RE.findall(line)]
    return [" ".join(segments)] if segments else []


def scrub_dump(d):
    """A dump without its view title, which can name the user."""
    return {k: v for k, v in d.items() if k != "title"}


def library_client(env):
    """The Web API client the library's Web calls go through: the personal one when a
    personal client ID is set in the environment, otherwise the shared one."""
    return "personal" if env.get("NCSPOT_CLIENT_ID", "").strip() else "shared"


def _toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    return json.dumps(str(v))


def render_config(values):
    top = [f"{k} = {_toml_value(v)}\n" for k, v in values.items() if not isinstance(v, dict)]
    tables = []
    for k, v in values.items():
        if isinstance(v, dict):
            body = "".join(f"{kk} = {_toml_value(vv)}\n" for kk, vv in v.items())
            tables.append(f"[{k}]\n{body}")
    return "".join(top) + "".join(tables)


def exit_code(results):
    if "FAIL" in results:
        return 1
    if "BLOCKED" in results:
        return 3
    return 0


# --- the running instance ---


class Instance:
    def __init__(self, binary, config, keep):
        self.binary = binary
        self.config = config
        self.keep = keep
        self.closed = False
        self.last = None
        self.base_dump = None
        self.tmp = tempfile.mkdtemp(prefix="ncspot-drive-")
        self.base = os.path.join(self.tmp, "base")
        self.run = os.path.join(self.tmp, "run")
        self.log = os.path.join(self.tmp, "ncspot.log")
        self.dump_path = os.path.join(self.tmp, "dump.json")
        self.sock = os.path.join(self.run, "ncspot", "ncspot.sock")
        self.server = f"ncspot-drive-{os.getpid()}"
        self.username = None
        self.tokens = []  # (real path, temp path, bytes copied)
        self.pid = None
        self.seq = 0
        self.cache_counts = {}

    def start(self):
        """Set up files and launch ncspot. Separate from __init__ so the caller
        holds the instance (and can close it) even when this raises."""
        self._setup_files(self.config)
        self._start(self.binary)

    def _setup_files(self, config):
        creds = os.path.join(REAL_CACHE, "librespot", "credentials.json")
        if not os.path.exists(creds):
            raise SetupError("no cached login: run ncspot once and log in")
        cache = os.path.join(self.base, ".cache")
        os.makedirs(os.path.join(cache, "librespot"))
        shutil.copy2(creds, os.path.join(cache, "librespot", "credentials.json"))
        with open(creds) as f:
            self.username = json.load(f).get("username")
        for token in glob.glob(os.path.join(REAL_CACHE, "rspotify_token*.json")):
            temp = os.path.join(cache, os.path.basename(token))
            self.tokens.append((token, temp, copy_token(token, temp)))
        # Library caches and user state keep the startup sync incremental instead of
        # a full sync per scenario against the account. Contents are never printed.
        for name in (
            "albums.db",
            "artists.db",
            "playlists.db",
            "tracks.db",
            "shows.db",
            "episodes.db",
        ):
            src = os.path.join(REAL_CACHE, name)
            if os.path.exists(src):
                shutil.copy2(src, cache)
        # Row counts of the copied caches, for the library scenarios. Counts only.
        for tab_name, name in CACHE_FILES.items():
            path = os.path.join(cache, name)
            if os.path.exists(path):
                try:
                    with open(path) as f:
                        self.cache_counts[tab_name] = len(json.load(f))
                except (OSError, ValueError, TypeError):
                    pass
        os.makedirs(os.path.join(self.base, ".config"))
        state = os.path.join(REAL_CONFIG, "userstate.cbor")
        if os.path.exists(state):
            shutil.copy2(state, os.path.join(self.base, ".config"))
        with open(os.path.join(self.base, ".config", "config.toml"), "w") as f:
            f.write(render_config(config))
        os.makedirs(self.run, mode=0o700)

    def tmux(self, *args, check=True):
        tmux = shutil.which("tmux")
        cmd = [tmux] if tmux else ["nix", "shell", "nixpkgs#tmux", "-c", "tmux"]
        try:
            return subprocess.run(
                cmd + ["-L", self.server, *args], check=check, capture_output=True, text=True
            )
        except (subprocess.CalledProcessError, FileNotFoundError, OSError) as e:
            raise SetupError(f"tmux failed: {type(e).__name__}") from e

    def _start(self, binary):
        env = f"XDG_RUNTIME_DIR={shlex.quote(self.run)}"
        pulse = os.path.join(os.environ.get("XDG_RUNTIME_DIR", ""), "pulse", "native")
        if os.environ.get("XDG_RUNTIME_DIR") and os.path.exists(pulse):
            env += f" PULSE_SERVER={shlex.quote('unix:' + pulse)}"
        env_cmd = (
            f"{env} exec {shlex.quote(binary)} -b {shlex.quote(self.base)} "
            f"-d {shlex.quote(self.log)}"
        )
        self.tmux("new-session", "-d", "-s", "ncspot", "-x", "200", "-y", "50", env_cmd)
        self.tmux("set-option", "-t", "ncspot", "remain-on-exit", "on", check=False)
        pid = self.tmux("display-message", "-p", "-t", "ncspot", "#{pane_pid}", check=False)
        self.pid = int(pid.stdout.strip()) if pid.stdout.strip().isdigit() else None
        deadline = time.monotonic() + SOCKET_WAIT
        while not os.path.exists(self.sock):
            if time.monotonic() > deadline:
                raise Fail("ncspot never came up (no socket after 15 s)")
            self.check_alive()
            time.sleep(0.2)

    def check_alive(self):
        out = self.tmux("list-panes", "-t", "ncspot", "-F", "#{pane_dead}", check=False)
        if out.returncode != 0 or out.stdout.strip() == "1":
            raise Fail("ncspot exited")

    def send(self, line):
        self.check_alive()
        s = socket.socket(socket.AF_UNIX)
        try:
            s.settimeout(5)
            s.connect(self.sock)
            # ncspot writes a status line on connect; if we close before it is
            # written, the server drops the connection before reading our line.
            data = b""
            while not data.endswith(b"\n"):
                chunk = s.recv(65536)
                if not chunk:
                    break
                data += chunk
            s.sendall((line + "\n").encode())
        except OSError as e:
            raise Fail(f"socket gone: {e}") from e
        finally:
            s.close()

    def status(self):
        """The current IPC status line (sent on connect)."""
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(5)
        try:
            try:
                s.connect(self.sock)
                data = b""
                while not data.endswith(b"\n"):
                    chunk = s.recv(65536)
                    if not chunk:
                        break
                    data += chunk
            except OSError as e:
                raise Fail(f"socket gone: {e}") from e
            return json.loads(data.decode() or "null")
        finally:
            s.close()

    def dump(self):
        self.send(f"harness dump {self.dump_path}")
        deadline = time.monotonic() + DUMP_WAIT
        while time.monotonic() < deadline:
            try:
                with open(self.dump_path) as f:
                    d = json.load(f)
                if d["seq"] > self.seq:
                    self.seq = d["seq"]
                    self.last = d
                    return d
            except (OSError, ValueError):
                pass
            time.sleep(0.1)
        raise Fail("dump stale: UI thread stuck or dump broken")

    def wait_settled(self, timeout):
        deadline = time.monotonic() + timeout
        while True:
            d = self.dump()
            if settled(d):
                return d
            if time.monotonic() > deadline:
                raise Fail(f"never settled within {timeout} s")
            time.sleep(0.25)

    def wait_idle(self, timeout):
        """Bounded wait for the startup sync to finish and go idle and its rate limit to lapse;
        never fails. The dump it ends on is the baseline scenarios take counter deltas from."""
        deadline = time.monotonic() + timeout
        while True:
            d = self.dump()
            if (
                d.get("library_done")
                and d["in_flight"] == 0
                and (d.get("quiet_ms") or 0) >= QUIET_MS
                and not any(d["throttled"].values())
            ) or (
                time.monotonic() > deadline
            ):
                self.base_dump = d
                return d
            time.sleep(1.0)

    def wait_library(self, timeout, blocked_on=()):
        """Poll dumps until the library sync is done and the views are settled. A timeout while
        a client in `blocked_on` is rate limited is BLOCKED, not FAIL."""
        deadline = time.monotonic() + timeout
        while True:
            d = self.dump()
            if d.get("library_done") and settled(d):
                return d
            if time.monotonic() > deadline:
                blocked_if_throttled(d, blocked_on)
                raise Fail(f"library sync not done within {timeout} s")
            time.sleep(0.5)

    def load_fully(self, timeout, tab=0):
        d = self.wait_settled(timeout)
        stalls = 0
        while d["tabs"][tab]["max"] is not None and d["tabs"][tab]["rows"] < d["tabs"][tab]["max"]:
            before = d["tabs"][tab]["rows"]
            self.send("move bottom")
            d = self.wait_settled(timeout)
            stalls = stalls + 1 if d["tabs"][tab]["rows"] == before else 0
            if stalls >= 2:
                break
        return d

    def evidence(self):
        lines = []
        try:
            with open(self.log, errors="replace") as f:
                lines = f.read().splitlines()[-40:]
        except OSError:
            pass
        pane = self.tmux("capture-pane", "-p", "-t", "ncspot", check=False).stdout
        out = ["--- log tail ---", *redact(lines, self.username)]
        out += ["--- status bar ---", *redact(status_segments(pane.splitlines()), self.username)]
        bt = os.path.join(self.base, ".cache", "backtrace.log")
        if os.path.exists(bt):
            with open(bt, errors="replace") as f:
                out += ["--- backtrace.log ---", *redact(f.read().splitlines(), self.username)]
        if self.last:
            out += ["--- last dump ---", *redact([json.dumps(scrub_dump(self.last))], self.username)]
        return out

    def _write_back_tokens(self):
        for real, temp, copied in self.tokens:
            try:
                with open(temp, "rb") as f:
                    new = f.read()
            except OSError:
                new = None
            try:
                with open(real, "rb") as f:
                    now = f.read()
            except OSError:
                now = None
            if not should_write_back(copied, new, now):
                continue
            try:
                write_atomic(real, new)
                print("  updated a cached Web API token")
            except OSError:
                pass

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            self.tmux("kill-server", check=False)
        except SetupError:
            pass
        # kill-server only sends SIGHUP; make sure a wedged ncspot is really gone.
        if self.pid:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except OSError:
                pass
        # The temp dir holds credential copies: remove it even if the write-back
        # is interrupted.
        try:
            self._write_back_tokens()
        finally:
            if self.keep:
                print(f"  kept {self.tmp}")
            else:
                shutil.rmtree(self.tmp, ignore_errors=True)


# --- checks ---


def expect(cond, what, actual):
    if not cond:
        raise Fail(f"expected {what}, got {actual}")


def blocked_if_throttled(d, clients):
    hit = [c for c in clients if d["throttled"].get(c)]
    if hit:
        raise Blocked(f"rate limited: {', '.join(hit)}")


def area_delta(inst, d, area):
    """An area's and the Web API counters since the post-startup baseline."""
    b = inst.base_dump
    bm, m = b["areas"][area], d["areas"][area]
    return {
        "internal_ok": m["internal_ok"] - bm["internal_ok"],
        "fallback": m["fallback"] - bm["fallback"],
        "web": (d["web"]["shared"] + d["web"]["personal"])
        - (b["web"]["shared"] + b["web"]["personal"]),
    }


def delta(inst, d):
    """Metadata and Web API counters since the post-startup baseline."""
    return area_delta(inst, d, "metadata")


def tab(d, name):
    for t in d["tabs"]:
        if t["name"] == name:
            return t
    raise Fail(f"no tab {name!r} in {[t['name'] for t in d['tabs']]}")


# --- scenarios: (config, function(instance, args)) ---


def sc_album(inst, args):
    inst.send(f"harness open spotify:track:{TRACK}")
    d = inst.wait_settled(args.settle_timeout)
    blocked_if_throttled(d, ["internal"])
    expect(d["view"] == "album", "an album view", d["view"])
    t = tab(d, "Tracks")
    expect(t["rows"] >= 1 and t["rows"] == t["max"], "all tracks loaded", t)
    m = delta(inst, d)
    expect(m["internal_ok"] >= 1 and m["fallback"] == 0, "metadata on the internal API", m)


def sc_artist(inst, args):
    inst.send(f"harness open spotify:artist:{ARTIST}")
    d = inst.wait_settled(args.settle_timeout)
    blocked_if_throttled(d, ["internal", "shared", "personal"])
    expect(d["view"] == "artist", "an artist view", d["view"])
    top = tab(d, "Top 10")
    expect(top["rows"] >= 1, "top tracks loaded", top)
    albums = tab(d, "Albums")
    expect(albums["rows"] >= 1, "albums loaded", albums)


def sc_show(inst, args):
    inst.send(f"harness open spotify:show:{SHOW}")
    d = inst.load_fully(args.settle_timeout)
    blocked_if_throttled(d, ["internal"])
    expect(d["view"] == "show", "a show view", d["view"])
    t = d["tabs"][0]
    expect(t["max"] is not None, "a known episode total", t)
    expect(
        t["rows"] > 50 and t["rows"] <= t["max"] and t["rows"] * 100 >= t["max"] * 99,
        "every episode page loaded",
        t,
    )


def sc_removed(inst, args):
    inst.send(f"harness open spotify:episode:{REMOVED_EPISODE}")
    d = inst.wait_settled(args.settle_timeout)
    blocked_if_throttled(d, ["internal", "shared", "personal"])
    m = delta(inst, d)
    expect(m["fallback"] >= 1, "a Web API fallback", m)
    expect(not d["areas"]["metadata"]["latched"], "the area not latched", d["areas"]["metadata"])


def _web_only(inst, args):
    inst.send(f"harness open spotify:track:{TRACK}")
    d = inst.wait_settled(args.settle_timeout)
    blocked_if_throttled(d, ["shared", "personal"])
    expect(d["view"] == "album", "an album view", d["view"])
    m = delta(inst, d)
    expect(m["internal_ok"] == 0 and m["fallback"] == 0, "zero internal calls", m)
    expect(m["web"] >= 1, "Web API calls", m)


def sc_play_start(inst, args):
    """Open the track, play it and wait until it is playing; returns the status."""
    inst.send(f"harness open spotify:track:{TRACK}")
    inst.wait_settled(args.settle_timeout)
    inst.send("play")
    deadline = time.monotonic() + args.settle_timeout
    while True:
        st = inst.status()
        if st and st.get("playable") and "Playing" in json.dumps(st.get("mode")):
            break
        if time.monotonic() > deadline:
            raise Fail(f"playback never started: {st and st.get('mode')}")
        time.sleep(0.5)
    expect(bool(st["playable"]), "metadata of the playing track", st["playable"])
    return st


def sc_play(inst, args):
    sc_play_start(inst, args)
    inst.send("stop")


def sc_library(inst, args):
    inst.send("update")
    d = inst.wait_library(args.settle_timeout)
    inst.send("harness open library")
    d = inst.wait_settled(args.settle_timeout)
    blocked_if_throttled(d, ["internal"])
    for name in LIBRARY_TABS:
        t = tab(d, name)
        want = inst.cache_counts.get(name)
        if want:
            expect(t["rows"] * 100 >= want * 99, f"{name} at 99% of its cache ({want})", t)
        else:
            expect(t["rows"] > 0, f"{name} non-empty", t)
    m = area_delta(inst, d, "library")
    expect(m["internal_ok"] >= 1 and m["fallback"] == 0, "library on the internal API", m)


def sc_library_delta(inst, args):
    inst.send("update")
    d = inst.wait_library(args.settle_timeout)
    blocked_if_throttled(d, ["internal"])
    m = area_delta(inst, d, "library")
    expect(
        1 <= m["internal_ok"] <= 6 and m["fallback"] == 0,
        "a delta sync of 1 to 6 internal requests, no fallback",
        m,
    )


def sc_library_area_off(inst, args):
    # Only the client the library's Web calls use: Browse and other startup calls always go
    # through the shared one, and its rate limit says nothing about the library.
    client = library_client(os.environ)
    inst.send("update")
    d = inst.wait_library(args.settle_timeout, blocked_on=[client])
    blocked_if_throttled(d, [client])
    inst.send("harness open library")
    d = inst.wait_settled(args.settle_timeout)
    m = area_delta(inst, d, "library")
    expect(m["internal_ok"] == 0, "no library calls on the internal API", m)
    expect(m["web"] >= 1, "Web API library calls", m)
    expect(all(t["name"] != "Your Episodes" for t in d["tabs"]), "no Your Episodes tab", d["tabs"])


WRITE_TARGETS = [
    ("track", f"spotify:track:{TRACK}"),
    ("artist", f"spotify:artist:{ARTIST}"),
    ("show", f"spotify:show:{SHOW}"),
]


def sc_library_write(inst, args):
    """Save then remove items not already in the library, through the internal API. `harness
    save/unsave` check that the library changed, so a clean error field is the per-item check;
    the counters show each write was one internal call with no fallback. Row counts are not
    used: the Artists tab also lists unfollowed artists of saved tracks."""
    inst.send("update")
    d = inst.wait_library(args.settle_timeout, blocked_on=["internal"])
    blocked_if_throttled(d, ["internal"])
    wrote = 0
    for kind, uri in WRITE_TARGETS:
        inst.send(f"harness save {uri}")
        d = inst.wait_settled(args.settle_timeout)
        if d.get("error") and "already in the library" in d["error"]:
            print(f"  skip {kind}: test item already in the library")
            continue
        expect(d.get("error") is None, f"{kind}: save without error", d.get("error"))
        inst.send(f"harness unsave {uri}")
        d = inst.wait_settled(args.settle_timeout)
        expect(d.get("error") is None, f"{kind}: remove without error", d.get("error"))
        wrote += 2
    blocked_if_throttled(d, ["internal"])
    m = area_delta(inst, d, "library")
    expect(m["fallback"] == 0, "no library fallback", m)
    expect(m["internal_ok"] >= wrote, "one internal call per write", m)


def sc_reconnect(inst, args):
    sc_play_start(inst, args)
    before = inst.dump()["player"]["position_ms"]
    inst.send("harness drop-session")
    deadline = time.monotonic() + 45
    while True:
        d = inst.dump()
        if (
            d["session"]["reconnects"] >= 1
            and d["player"]["status"] == "playing"
            and d["player"]["position_ms"] > before
        ):
            break
        if time.monotonic() > deadline:
            raise Fail(f"no reconnect within 45s: {d['session']} {d['player']}")
        time.sleep(1)
    inst.send("stop")


SCENARIOS = {
    "album": ({}, sc_album, False),
    "artist": ({}, sc_artist, False),
    "show": ({}, sc_show, False),
    "removed": ({}, sc_removed, False),
    "internal_off": ({"internal_api": False}, _web_only, False),
    "area_off": ({"internal_api_areas": {"metadata": False}}, _web_only, False),
    "play": ({}, sc_play, True),
    "library": ({}, sc_library, False),
    "library_delta": ({}, sc_library_delta, False),
    "library_area_off": ({"internal_api_areas": {"library": False}}, sc_library_area_off, False),
    "library_write": ({}, sc_library_write, False),
    "reconnect": ({}, sc_reconnect, True),
}


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("scenarios", nargs="*")
    p.add_argument("--play", action="store_true", help="also run playback scenarios")
    p.add_argument("--keep", action="store_true", help="keep the temp dirs")
    p.add_argument("--no-build", action="store_true", help="skip cargo build")
    p.add_argument("--settle-timeout", type=float, default=30.0)
    args = p.parse_args(argv)

    names = args.scenarios or [n for n, (_, _, play) in SCENARIOS.items() if args.play or not play]
    unknown = [n for n in names if n not in SCENARIOS]
    if unknown:
        print(f"unknown scenario(s): {', '.join(unknown)}; known: {', '.join(SCENARIOS)}")
        return 2
    if not args.no_build:
        r = subprocess.run(["cargo", "build", "--features", "harness"], cwd=REPO)
        if r.returncode != 0:
            return 2
    binary = os.path.join(REPO, "target", "debug", "ncspot")

    results = []
    current = {"inst": None}

    def on_signal(signum, _frame):
        if current["inst"]:
            current["inst"].close()
        sys.exit(128 + signum)

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)

    for name in names:
        config, func, _ = SCENARIOS[name]
        inst = None
        try:
            inst = Instance(binary, config, args.keep)
            current["inst"] = inst
            inst.start()
            inst.wait_idle(args.settle_timeout * 2)
            func(inst, args)
            results.append("PASS")
            print(f"PASS     {name}")
        except SetupError as e:
            print(f"SETUP    {name}: {e}")
            return 2
        except Blocked as e:
            results.append("BLOCKED")
            print(f"BLOCKED  {name}: {e}")
        except Fail as e:
            results.append("FAIL")
            print(f"FAIL     {name}: {e}")
            if inst:
                print("\n".join("  " + line for line in inst.evidence()))
        except Exception as e:
            results.append("FAIL")
            print(f"FAIL     {name}: unexpected {type(e).__name__}: {e}")
            if inst:
                print("\n".join("  " + line for line in inst.evidence()))
        finally:
            if inst:
                inst.close()
            current["inst"] = None
    return exit_code(results)


if __name__ == "__main__":
    sys.exit(main())

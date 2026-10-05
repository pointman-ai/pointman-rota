"""Signing a Claude account in from the apps (MB-216; James, thread 2850 [3021]): rota runs Claude Code's
own `claude setup-token`, hands the node its sign-in link, and, once the person has signed in on any
device and pasted back the code the page showed, hands the node the token for the vault.

- **The link:** setup-token prints two. One sends the browser back to this machine's localhost, so it
  only works here; the other sends it to platform.claude.com, which shows a code to paste back. That's
  the one given out. The browser setup-token would open on this machine is stopped.
- **The token** comes only out of `code`, as the payload `/secrets add` makes, for the node to seal into
  the vault. Nothing here writes it anywhere, logs it or keeps it.
- **A sign-in waits** 15 minutes for its code, then it's dropped.
"""

from __future__ import annotations

import json
import os
import pty
import re
import select
import signal
import struct
import tempfile
import termios
import fcntl
import threading
import time
from pathlib import Path

from rota import accounts, probe

WAIT_SECONDS = 15 * 60
ANSI = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]")
LINK = re.compile(r"https://[^\s\x07\x1b\]]+")
CODE = re.compile(r"[A-Za-z0-9#_.~-]{8,512}")


class SignInError(Exception):
    pass


class Session:
    def __init__(self, pid: int, fd: int, folder: str):
        self.pid, self.fd, self.folder = pid, fd, folder
        self.started = time.monotonic()
        self.raw = b""

    def read(self, until, timeout: float) -> str:
        """Read the program's output until `until(text)` is true (or it ends, or the timeout)."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            text = self.text()
            if until(text):
                return text
            ready, _, _ = select.select([self.fd], [], [], 0.25)
            if ready:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.raw += chunk
        return self.text()

    def text(self) -> str:
        return ANSI.sub("", self.raw.decode("utf-8", "replace"))

    def end(self) -> None:
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        try:
            os.close(self.fd)
        except OSError:
            pass
        _remove(self.folder)


def _remove(folder: str) -> None:
    for p in sorted(Path(folder).rglob("*"), reverse=True):
        try:
            p.unlink() if not p.is_dir() else p.rmdir()
        except OSError:
            pass
    try:
        Path(folder).rmdir()
    except OSError:
        pass


def manual_link(raw: str) -> str | None:
    """The sign-in link that works on any device: the one whose callback isn't this machine's localhost.
    (A link can come as a terminal hyperlink, so it's read from the raw output too.)"""
    for url in LINK.findall(raw):
        if "redirect_uri=" in url and "localhost" not in url and "127.0.0.1" not in url:
            return url
    return None


class SignIns:
    def __init__(self, claude: str | None = None):
        self.claude = claude
        self.open: dict[str, Session] = {}
        self.lock = threading.Lock()

    def start(self, session_id: str, timeout: float = 60) -> dict:
        """Start setup-token; answer its sign-in link."""
        claude = self.claude or probe.find_claude()
        if not claude:
            raise SignInError("Claude Code isn't installed on this machine")
        self.sweep()
        folder = tempfile.mkdtemp(prefix="rota-signin-")
        # a stand-in for `open` and `xdg-open`: the sign-in happens wherever the person is, not here
        for name in ("open", "xdg-open"):
            stub = Path(folder) / name
            stub.write_text("#!/bin/sh\nexit 0\n")
            stub.chmod(0o700)
        env = {k: v for k, v in os.environ.items() if k not in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN",
                                                                "CLAUDE_CODE_OAUTH_TOKEN")}
        env.update({"PATH": f"{folder}:{env.get('PATH', '/usr/bin:/bin')}", "BROWSER": f"{folder}/open",
                    "TERM": "dumb", "NO_COLOR": "1"})
        pid, fd = pty.fork()
        if pid == 0:                                   # the child: Claude Code's own sign-in
            try:
                os.chdir(folder)
                os.execvpe(claude, [claude, "setup-token"], env)
            finally:
                os._exit(127)
        # a wide terminal, so the link and the token aren't wrapped onto several lines
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 4000, 0, 0))
        s = Session(pid, fd, folder)
        s.read(lambda _: manual_link(s.raw.decode("utf-8", "replace")) is not None, timeout)
        link = manual_link(s.raw.decode("utf-8", "replace"))
        if link is None:
            s.end()
            raise SignInError("Claude Code didn't give a sign-in link")
        with self.lock:
            old = self.open.pop(session_id, None)
            self.open[session_id] = s
        if old is not None:
            old.end()
        return {"url": link, "prompt": "Sign in as the account to add, then paste the code the page shows."}

    def code(self, session_id: str, code: str, timeout: float = 120) -> dict:
        """Give setup-token the code; answer the token as the vault payload. The session ends either way."""
        code = (code or "").strip()
        if not CODE.fullmatch(code):
            raise SignInError("that isn't a sign-in code: paste the code the page showed")
        with self.lock:
            s = self.open.pop(session_id, None)
        if s is None:
            raise SignInError("this sign-in has ended: start it again")
        try:
            before = len(s.text())
            os.write(s.fd, code.encode() + b"\r")
            text = s.read(lambda t: bool(accounts.TOKEN.search(t)) or _failed(t[before:]), timeout)
            found = accounts.TOKEN.search(text)
            if not found:
                raise SignInError("Claude didn't take that code: start the sign-in again" if _failed(text)
                                  else "Claude Code didn't give a token")
            return {"value": json.dumps({"kind": "env", "vars": {"CLAUDE_CODE_OAUTH_TOKEN": found[0]}})}
        finally:
            s.end()

    def sweep(self) -> None:
        """Drop sign-ins nobody finished."""
        with self.lock:
            stale = [k for k, s in self.open.items() if time.monotonic() - s.started > WAIT_SECONDS]
            gone = [self.open.pop(k) for k in stale]
        for s in gone:
            s.end()


def _failed(text: str) -> bool:
    low = text.lower()
    return any(w in low for w in ("invalid", "error", "failed", "expired"))

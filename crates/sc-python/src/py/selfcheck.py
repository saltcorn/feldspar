"""`feldspar python check`: start-up questions about the embedded interpreter.

Run in the server's own interpreter, outside the import gate — this is the
operator's question about the install, not a body — and with no database. Each
check is a name, whether it passed, and one line of detail; the caller prints
them and exits non-zero when any failed.

The four imports are the ones that load C extensions from `lib-dynload` and the
libraries behind them (OpenSSL, SQLite, libffi) or read the host's data
(the time-zone database), so they are where a bundled CPython on a foreign
distribution would fail first. The HTTPS request is the certificate question:
python-build-standalone's OpenSSL has a compiled-in certificate path, and
whether the host keeps its bundle there is a per-distribution answer
(TODO-portable-release D10).

The `libc` answer is which `libc.so.6` this process has mapped: the host's,
or the bundle's under `glibc/` on a musl host (R2.1). The release smoke test
reads it on every image, and an operator asking "which libc is this using?"
gets the file rather than a guess. `imports` are further modules to import —
the smoke test's check that a wheel pip installed into the environment loads
in the embedded interpreter (R2.4).
"""

import importlib
import json
import sys


def mapped_libc():
    """The `libc.so.6` (or musl's `ld-musl-*`) in `/proc/self/maps`, or "" off Linux."""
    try:
        with open("/proc/self/maps") as maps:
            for line in maps:
                # address, perms, offset, device, inode and — for a file — its path.
                fields = line.split(None, 5)
                path = fields[5].strip() if len(fields) == 6 else ""
                name = path.rsplit("/", 1)[-1]
                if name.startswith("libc.so") or name.startswith("libc-") or name.startswith("ld-musl-"):
                    return path
    except OSError:
        pass
    return ""


def run(url, imports=()):
    checks = []

    def check(name, fn):
        try:
            checks.append([name, True, str(fn() or "")])
        except BaseException as e:  # noqa: BLE001 — every failure is a result here
            checks.append([name, False, "%s: %s" % (type(e).__name__, e)])

    def _ssl():
        import ssl

        paths = ssl.get_default_verify_paths()
        return "%s; certificates from %s (file) and %s (directory)" % (
            ssl.OPENSSL_VERSION,
            paths.cafile or paths.openssl_cafile,
            paths.capath or paths.openssl_capath,
        )

    def _sqlite3():
        import sqlite3

        return "SQLite %s" % sqlite3.sqlite_version

    def _ctypes():
        import ctypes

        # The process itself, through libffi: a dlopen that needs no library
        # name, so it says nothing about the host's other libraries.
        ctypes.CDLL(None)
        return "libffi loaded"

    def _zoneinfo():
        import zoneinfo

        return "time zones from %s" % (":".join(zoneinfo.TZPATH) or "the tzdata package only")

    check("import ssl", _ssl)
    check("import sqlite3", _sqlite3)
    check("import ctypes", _ctypes)
    check("import zoneinfo", _zoneinfo)

    for name in imports:
        check(
            "import %s" % name,
            lambda name=name: getattr(importlib.import_module(name), "__file__", None) or "built in",
        )

    if url:

        def _https():
            import ssl
            import urllib.request

            context = ssl.create_default_context()
            with urllib.request.urlopen(url, timeout=15, context=context) as response:
                return "%s answered %s, certificate verified" % (url, response.status)

        check("https", _https)

    return json.dumps(
        {
            "version": "%d.%d.%d" % sys.version_info[:3],
            "home": sys.prefix,
            "executable": sys.executable,
            "libc": mapped_libc(),
            "checks": checks,
        }
    )

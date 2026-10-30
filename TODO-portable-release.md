# Saltcorn v2 — One release for every Linux distribution, with Python (milestones R1–R4)

Today's release is two artifacts (`scripts/build-static.sh`): a **static** one with no Python
that runs anywhere, Alpine included, and a **`debian13-python`** one that runs only on Debian 13.
This list replaces them with **one artifact, with working Python**, that runs on:

- Fedora (recent) and RHEL 10 and its derivatives (Alma, Rocky, …)
- Debian 13, Ubuntu (24.04 and later) and their derivatives (Mint, MX Linux, …)
- Arch and its derivatives (CachyOS, Manjaro, EndeavourOS)
- openSUSE Leap 16 and Tumbleweed
- Alpine

on **x86_64 and aarch64**, plus a separate build for **macOS on Apple silicon** (R4). Windows is
served by the Linux artifact under WSL2; the BSDs and 32-bit ARM are out of reach (D14).

The aim is the **least work per release**: per platform, one build, one tarball and one
smoke-test run, each done by CI. The design comes first, then the decisions and why they were
made, then the tasks. Milestones are prefixed **R**; work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. Why `debian13-python` runs only on Debian 13

It is tied to the host in three ways:

1. **glibc.** It is built on trixie, so it needs glibc ≥ 2.41. RHEL 10, Ubuntu 24.04 and Mint 22
   have 2.39.
2. **The `libpython` soname.** It links `libpython3.13.so.1.0` from the host. System Pythons on the
   target list span 3.12 (RHEL 10, Ubuntu 24.04), 3.13 (Debian 13, Leap 16) and 3.14 (Fedora,
   Arch), and the rolling distributions keep moving. `abi3-py311` (`Cargo.toml:357`) does not
   help: an *embedding* binary on Linux still links one versioned `libpython`.
3. **The installers.** `install.sh` knows Debian-family and Alpine, `setup-host.sh` knows only
   apt, and the packages they add (`libpython3.13`, `python3-venv`) are Debian names.

Alpine cannot get Python from a musl build either. V8 has no musl archive (`build-static.sh`
explains why), and a static binary cannot load CPython's extension modules or any compiled wheel.

## 2. The artifact

One tarball per architecture, built on **Debian 12 (bookworm, glibc 2.36)**, dynamically linked,
with `--features python`:

```
/opt/feldspar/bin/feldspar              # loader: the host's /lib64/ld-linux-x86-64.so.2
/opt/feldspar/bin/feldspar.glibc        # the same file, loader set to the bundled one
/opt/feldspar/python/                   # python-build-standalone CPython 3.13, pinned
    bin/python3, bin/python3.glibc      #   the interpreter pip and venv run under (×2, as above)
    lib/libpython3.13.so                #   what bin/feldspar links, via RUNPATH $ORIGIN/../python/lib
    lib/python3.13/                     #   the stdlib, lib-dynload, pip, ensurepip
/opt/feldspar/glibc/                    # bookworm's ld.so, libc, libm, libgcc_s and glibc's own
                                        #   NSS modules (compat, db) — used only by the .glibc copies
/opt/feldspar/ui/…, plugins/, install.sh, setup-host.sh, VARIANT   # as today
```

**`install.sh` picks the libc when it installs:**

- On a **glibc host** with glibc ≥ 2.36 (every target except Alpine), the normal executables stay.
  The process uses the **host's glibc**, which the distribution keeps patched. The only thing it
  brings of its own is CPython.
- On a **musl host** (Alpine), or a glibc host whose glibc is too old to start the normal binary,
  `install.sh` moves `feldspar.glibc` over `feldspar` and `python3.glibc` over `python3`. The
  paths everything else names (systemd units, the hosted setup's `ExecStart`, `--python-bin`)
  stay the same.

The executables are copied, not wrapped. A wrapper that runs `ld.so` explicitly would make
`/proc/self/exe` the loader, which breaks CPython's own path search and anything else that asks
where the program is.

## 3. How the embedded interpreter finds its stdlib

Today this works by accident. `Python::initialize()` (`crates/sc-python/src/interp.rs:174`)
sets no program name, so CPython uses `python3`, finds `/usr/bin/python3` on `PATH`, and looks for
the stdlib beside it. With a bundled CPython and no system Python, that search fails, falls back
to the prefix PBS was built at (`/install`), and the interpreter dies with "No module named
encodings".

So `sc-python` must start the interpreter with an explicit **home** of `<exe>/../python`
(`std::env::current_exe`, symlinks resolved) when that directory exists. It keeps today's behaviour
when it does not, which covers developer builds and `cargo test`. Two constraints:

- **No `PYTHONHOME` in the process environment.** It would be inherited by every child: npm,
  Chromium, the coding agent's shell, and any *other* Python those start.
- **The limited API has no `PyConfig`.** Use `PyConfig.home` + `Py_InitializeFromConfig`, which
  means turning `abi3` off for this crate (it buys nothing when embedding one pinned version) or
  declaring those few FFI functions by hand. `Py_SetPythonHome` is deprecated and is not the way.

**`--python-bin` defaults to `<exe>/../python/bin/python3`** when it exists, else `python3` as
today (`crates/sc-python/src/env.rs:55`). That way pip and the embedded interpreter are always the
same version, and the ABI check (`abi_refusal`) cannot trip on a fresh host.

## 4. What stays the same

- **Memory.** The binary, `libpython`, the stdlib and (on Alpine) the bundled glibc are read-only
  files under one shared prefix. Every instance on a host shares one copy of their pages,
  whichever user it runs as, exactly as with the system `libpython` today. Per-instance memory
  (the Python heap, V8 isolates) is unchanged, and an instance that never runs Python never starts
  the interpreter. This matters for `~/feldspar-hosted`, which packs many instances onto one
  machine.
- **The install prefix** is still compiled in (`SC_BUNDLE_PREFIX`), and it is what makes the
  absolute loader path in the `.glibc` copies possible.
- **Python 3.13.** Venvs that exist today were made by Debian's `python3.13`. They pass the boot
  check against the bundled 3.13, and their cp313 wheels load in it (§5, D4).

---

# Part II — Decisions

**D1. Bundle CPython ([python-build-standalone](https://github.com/astral-sh/python-build-standalone))
instead of linking the host's.**
*Alternatives:* one build per distribution (6–8 per release, and every Arch or Tumbleweed Python
bump breaks one), or one per Python minor version with the installer choosing by the host's
`python3` (3 per release, one more each year, and the libpython and venv package names per
distribution are still needed).
*Why:* it is the only option that is **one build per release**. The host needs nothing
Python-related at all, and pip and the embedded interpreter can never disagree on a version.
*Cost:* the tarball grows by roughly 25–35 MB, and CPython security fixes reach hosts with
feldspar releases rather than with distribution updates: bump the pin (D7).

**D2. Build on bookworm (glibc 2.36).**
*Why:* it is the oldest glibc the target list needs to beat. RHEL 10 and Ubuntu 24.04 have 2.39.
The static variant already builds on bookworm, so `rusty_v8` and the rest of the toolchain are
known to work there.
*Cost:* the base has to move before bookworm's support ends (LTS until mid-2028).

**D3. Bundle glibc too, but use it only where the host's glibc cannot run the binary.**
*Why bundle it:* it is the only way to give Alpine Python (§1). gcompat is too incomplete to trust
under V8 and CPython.
*Why only there:* a process running the bundled glibc on a glibc distribution would
- miss the distribution's glibc security updates (today the hosted server gets those through
  unattended-upgrades);
- load the host's NSS modules (`systemd`, `myhostname`, `resolve`, `sss`; openSUSE's
  `passwd: compat`) and anything Python's `ctypes` opens through the host's `/etc/ld.so.cache`.
  Those were built against a newer glibc: most fail cleanly with a version error, but glibc's own
  modules use glibc-internal symbols and may not fail cleanly.

None of that applies on Alpine, which has no glibc libraries to pick up and a minimal or absent
`nsswitch.conf`.
*Consequence:* the `.glibc` executables use **`DT_RPATH`** (not `RUNPATH`) to
`/opt/feldspar/glibc` and `/opt/feldspar/python/lib`. `DT_RPATH` applies to every library loaded
at runtime, including glibc's own NSS modules, so those are found in the bundle before anywhere on
the host. The bundle ships glibc's `libnss_compat` and `libnss_db`. `files` and `dns` have been
inside libc since 2.34.

**D4. Stay on Python 3.13 for now.**
*Why:* compatibility with running installations (CLAUDE.md, "Status"). Existing venvs report
3.13, and moving to 3.14 makes every one of them trip `abi_refusal` until it is recreated and
everything reinstalled.
*Later:* a minor-version bump is its own release, with a note and a path that recreates the venv
(R1.6 is that path).

**D5. One artifact, published under the old names as well.**
*Why:* `docs/OPERATIONS.md`, existing hosts and `~/feldspar-hosted` fetch
`feldspar.tar.gz` and `feldspar-debian13-python.tar.gz` from the bucket. Upgrades must keep
working without anyone editing a URL. `VARIANT` says `linux`; `setup-host.sh` keeps accepting
`static` and `debian13-python` from old installs.

**D6. Detect musl explicitly; do not rely only on "does the normal binary start".**
*Why:* on an Alpine host with `gcompat` installed, `/lib64/ld-linux-x86-64.so.2` exists, so the
normal binary might start and then run on the shim. `install.sh` checks for `/lib/ld-musl-*` (or
`ldd --version` naming musl) first and only then probes the normal binary. A failing probe on a
glibc host means its glibc is older than 2.36: use the bundled one, and say so.

**D7. Pin PBS by release tag and SHA-256 in the script, like Rust and Node already are.**
*Why:* builds stay reproducible, and taking a CPython security release is a one-line change to the
pin.

**D8. Retire the `static` artifact only after the new one has shipped beside it.**
*Why:* the new artifact runs everywhere the static one does, so keeping both is pure release cost.
But the bundled-glibc path is new, so publish both for one or two releases. After that,
`feldspar.tar.gz` becomes the new artifact (D5).

**D9. `setup-host.sh` learns dnf, zypper and pacman; Alpine stays install-only.**
*Why:* without packages and a database the new distributions are not supported in practice. The
differences are a package table plus PostgreSQL's first-run setup (`postgresql-setup --initdb` on
Fedora/RHEL, `initdb` on Arch). Alpine runs OpenRC, not systemd, and `setup-host.sh` refuses it
today (`setup-host.sh:325`). Alpine keeps working the way it does now (`install.sh`, then the
operator's own service), and an OpenRC unit is a separate decision.

**D10. Certificates are checked, not assumed.**
*Why:* PBS's OpenSSL has a compiled-in certificate path, which may not be where Fedora/RHEL
(`/etc/pki/tls`) or openSUSE (`/etc/ssl/ca-bundle.pem`) keep theirs. pip is unaffected, because it
brings its own certifi; user code calling `urllib` or `ssl` is not. The smoke test makes a
verified HTTPS connection from the embedded interpreter on every image. Where that fails,
`setup-host.sh` writes `SSL_CERT_FILE` (the distribution's bundle) into the unit. Children
inheriting that is harmless, because it names the right file.

**D11. An install replaces the whole tree with renames, never overwrites files in place.**
*Why:* both installers are already safe to run twice, and running the curl installer again is
the documented upgrade, but neither works as an upgrade today:
- **A running server blocks it.** The tarball's `install.sh` `cp`s over `bin/feldspar`, and
  Linux refuses to write to a running executable ("Text file busy"), so `set -e` stops the
  install. `--deploy` stops `feldspar.service` first (`build-static.sh:1095`); the curl
  installer does not, yet ends by advising "run this installer again and restart the service".
  On `~/feldspar-hosted` every `feldspar@*` instance holds the binary open.
- **With this list's artifact it becomes dangerous.** Overwriting a mapped `libpython3.13.so`
  (or the bundled glibc) in place can crash every process that has it loaded, not just fail.
- **It is not atomic and leaves stale files.** A failure partway leaves a mix of versions, and
  files a release drops stay forever: old hashed assets under `ui/*/dist`, and modules removed
  from `plugins/`, which Settings → Modules keeps offering.
- **The curl installer always fetches `feldspar.tar.gz`** (`scripts/install.sh:36`), so
  rerunning it on a `debian13-python` host silently swaps in the build without Python. D5's
  single artifact ends this, but only for releases after it.

Unpacking to `${PREFIX}.new` and swapping with two renames fixes all of these. Files under the
prefix that no install put there are carried over into the new tree with a warning, not refused:
an operator's file must not block an upgrade, and they should hear that it is there. Running processes
keep the old inodes until they restart, an interrupted install leaves the old tree whole, and
the previous tree stays as `${PREFIX}.old`, so a rollback is one rename.

**D12. aarch64 Linux is the same artifact, built natively on an aarch64 machine.**
*Why:* prebuilt V8 and python-build-standalone both exist for `aarch64-unknown-linux-gnu`, and
the scripts already accept the target (`build-static.sh:155`, `:505`). It must be built *on*
aarch64: the module runtime's V8 startup snapshot is made by a build script on the build machine
and only works on that architecture, which is why `build-static.sh:313` refuses cross-builds.
Emulating ARM with qemu hits the same check and would be very slow anyway. A GitHub Actions
`ubuntu-24.04-arm` runner (or an ARM cloud VM) runs the same script and Dockerfile unchanged, and
because the machine is ARM, the distro smoke tests run instead of being skipped (`:701`).
It covers Graviton, Ampere, Raspberry Pi 4/5 on a 64-bit OS, Linux VMs on Apple silicon, Asahi,
WSL2 on Windows-on-ARM and ChromeOS's Linux. The bundled loader on Alpine is
`ld-linux-aarch64.so.1`.

**D13. Published file names carry the OS and architecture.**
`feldspar-linux-x86_64.tar.gz`, `feldspar-linux-aarch64.tar.gz`, `feldspar-macos-arm64.tar.gz`.
*Why:* today's names have none (`R2_OBJECT="feldspar.tar.gz"`, `build-static.sh:130`), so an
aarch64 `--release` would overwrite the x86_64 tarball. The old names (`feldspar.tar.gz`,
`feldspar-debian13-python.tar.gz`) stay as copies of the **x86_64** Linux tarball (D5), because
everything that fetches them today is x86_64. Each build machine publishes only its own names.

**D14. No build for 32-bit ARM, Windows or the BSDs.**
- **32-bit ARM** (armv7/armhf, armv6): no prebuilt V8, and 32-bit memory limits are tight for
  V8, CPython and Chromium. Every Raspberry Pi from the 3 onward runs a 64-bit OS.
- **Windows:** prebuilt V8 exists, but the code is unix-first (79 `cfg(unix)` against one
  `cfg(windows)`, plus systemd, Unix sockets, the coding agent's shell, CmdStan's process
  handling) and CI never builds it. WSL2 runs the Linux artifact as it is.
- **FreeBSD, OpenBSD, NetBSD:** no prebuilt V8. FreeBSD packages Deno only by building V8 from
  source with local patches, which would have to be redone on every V8 upgrade. A Linux VM
  (bhyve) is the route; FreeBSD's ability to run Linux binaries directly is an experiment, not
  something to support.

`install.sh` names each of these and the way forward (D16), rather than failing on a download.

**D15. macOS is Apple silicon only, built by its own script on a Mac.**
*Why a build at all:* the code base already builds and runs on Macs (README §4.2 is a supported
development setup), and the iOS build hosts (`TODO-build-hosts.md`) *are* Macs. Today they need a
Rust toolchain just to run `feldspar build-agent`; a download removes that.
*Why its own script:* it has to be built on a Mac, because cross-compiling V8 and the C
dependencies needs Apple's SDK. `scripts/build-macos.sh` builds, bundles python-build-standalone
(`@loader_path` instead of `$ORIGIN`), re-signs and packs. The bucket upload and the post-build
checks move into `scripts/lib/release.sh`, which both build scripts source, so `build-static.sh`
does not grow.
*Signing:* rewriting library paths with `install_name_tool` breaks the signature, and Apple
silicon refuses unsigned code, so the script re-signs ad hoc (`codesign -s -`). No notarization:
files fetched with `curl` don't get the quarantine flag that makes Gatekeeper check them.
*Not included:* Intel Macs (Apple's support for them is ending, and each one is another build),
and `setup-host.sh` (a Mac is a development machine or a build host, not a server;
`feldspar setup --dev` covers it).

**D16. `install.sh` never builds from source; where there is no binary, it says what to do.**
*Why:* where no binary exists (BSDs, 32-bit ARM, musl on other architectures, riscv64, ppc64le),
a source build fails too, at V8. Where prebuilt V8 exists, there is a binary, so a fallback would
never run. A source build also needs Rust, clang, cmake and Node, several GB of RAM, tens of GB
of disk and 30–60 minutes: a `curl | sh` that silently turns into that, and then runs out of
memory halfway, is worse than a clear message. The messages:
- 32-bit ARM: install the 64-bit OS;
- Windows (Git Bash, MSYS, Cygwin): `wsl --install -d Debian`, then the same `curl … | sh`;
- BSDs: run it in a Linux VM;
- Intel Mac: build from source (README §4.2);
- anything else: the platform and architecture it found, and the supported list.

**D17. Every shipped ELF file must load on 16 KB-page kernels.**
*Why:* the Raspberry Pi 5's default kernel and Asahi Linux use 16 KB pages, and a file laid out
for 4 KB pages does not load there. Rust's linker default for aarch64 allows 64 KB, so the
binary is probably fine; the bundled `libpython`, its extension modules and glibc are unknown.
Containers and emulators always use the host's page size, so the build asserts it with
`readelf -l` and one check runs on real hardware.

---

# Part III — Tasks

## R1 — The artifact

- [ ] R1.1 `static-build.Dockerfile`: the Python variant builds on bookworm (D2), not trixie. Drop
      the `python3-dev` layer. Download the pinned PBS 3.13 `install_only_stripped` tarball for
      the target (D7) and check its SHA-256. Unpack it at `${PREFIX}/python` *inside the build
      container*, so build and run paths agree. Build with
      `PYO3_PYTHON=${PREFIX}/python/bin/python3` and
      `-C link-arg=-Wl,-rpath,$ORIGIN/../python/lib`. **Check first** that the PBS release ships
      a shared `libpython3.13.so` on Linux GNU. If it does not, link its PIC
      `libpython3.13.a` with `-rdynamic` and drop `python/lib` from the runtime picture.
- [ ] R1.2 The `.glibc` copies (D3): copy `ld-linux-x86-64.so.2`, `libc.so.6`, `libm.so.6`,
      `libgcc_s.so.1`, `libnss_compat.so.2` and `libnss_db.so.2` from the build image to
      `${PREFIX}/glibc` (aarch64: its own loader name). Make `bin/feldspar.glibc` and
      `python/bin/python3.glibc` with `patchelf --set-interpreter ${PREFIX}/glibc/ld-linux-… 
      --force-rpath --set-rpath ${PREFIX}/glibc:${PREFIX}/python/lib`. Check with `readelf` in
      the script that each copy has the expected `PT_INTERP` and `DT_RPATH`, the way
      `assert_python_linkage` checks linkage today.
- [ ] R1.3 `build-static.sh`: the variants become `linux` (new) and `static` (kept until R3.3).
      `assert_python_linkage` asks for `libpython3.13.so` resolved from `$ORIGIN`, not from the
      host. Remove the generated libpython install step (`build-static.sh:769-790`).
- [ ] R1.4 `sc-python`: start the interpreter with `PyConfig.home = <exe>/../python` when that
      directory exists (§3), without setting `PYTHONHOME` in the environment. Test: a test binary
      with a fake `python/` beside it gets that home. A plain `cargo test` build without one
      starts as today.
- [ ] R1.5 `sc-python`: `--python-bin` defaults to the bundled interpreter when it exists (§3).
      The `--help` text (`sc-cli/src/main.rs:3477`) and `docs/tutorial-python.md` say so. Test:
      the default resolves to the bundled path when present and to `python3` otherwise.
- [ ] R1.6 An existing venv whose `pyvenv.cfg` `home` is not the bundled interpreter but has the
      same minor version (D4: Debian's 3.13 on an upgraded host) is repointed with
      `<bundled python3> -m venv --upgrade <dir>`. That keeps its site-packages, so the venv no
      longer depends on the host's `python3` (which an operator may then remove). A different
      minor version still gets `abi_refusal`'s sentence. Test with a fake `pyvenv.cfg`.
- [ ] R1.7 A self-check that starts the embedded interpreter without a database: an existing
      `feldspar check` mode if one fits, else a small subcommand. It imports `ssl`, `sqlite3`,
      `ctypes` and `zoneinfo`, opens a verified HTTPS connection, and prints the interpreter's
      version and home. The smoke test (R2.4) and operators both use it.

## R2 — Installing on every distribution

- [ ] R2.1 `install.sh`: detect musl (D6). On musl, or when the normal binary does not start, move
      the `.glibc` copies over the normal ones and log which libc this install uses. On glibc,
      delete the `.glibc` copies and `glibc/`, or keep them for a later host move (decide; keeping
      costs about 5 MB). Accept `ID`/`ID_LIKE` for Fedora, RHEL, SUSE and Arch, and install the
      few packages it needs there (ca-certificates, tar) with dnf, zypper and pacman. `--dry-run`
      shows the chosen libc.
- [ ] R2.2 `setup-host.sh` (D9): a package table per family (Debian, Fedora/RHEL, SUSE, Arch) for
      PostgreSQL, the client, ca-certificates, curl, Node and Chromium. PostgreSQL's first-run
      initialisation and service name per family. No Python packages anywhere. Keep accepting
      `VARIANT` = `static` or `debian13-python` from older installs.
- [ ] R2.3 `setup-host.sh`: write `SSL_CERT_FILE` into the unit where the self-check's HTTPS call
      fails without it (D10), on whichever families the smoke test shows need it.
- [ ] R2.4 The smoke test in `build-static.sh` runs the R1.7 self-check on `debian:13-slim`,
      `debian:12-slim`, `ubuntu:24.04`, `ubuntu:latest`, `fedora:latest`, `almalinux:10`,
      `archlinux:latest`, `opensuse/leap:16.0`, `opensuse/tumbleweed` and `alpine:3`, each after
      the tarball's own `install.sh`. It also checks:
      - on every glibc image, that the process maps the **host's** `libc.so.6`, not the bundle's
        (`/proc/<pid>/maps`);
      - on Alpine, that it maps the bundle's;
      - on Debian 13 and Alpine, that the bundled `python3 -m venv` + `pip install` of one
        pure-Python package and one manylinux wheel works, and the embedded interpreter imports
        both.

      Derivatives (Mint, MX, CachyOS, Manjaro, EndeavourOS, Rocky) are covered by `ID_LIKE`, not
      by images.
- [ ] R2.5 SELinux: on a Fedora or Alma VM with SELinux enforcing, `setup-host.sh` then a Python
      trigger works. V8 and CPython both need writable and executable memory, from a binary under
      `/opt`. Note the result in `docs/OPERATIONS.md`; make a fix only if it fails.
- [ ] R2.6 Install by swapping trees (D11). The tarball's `install.sh` (generated in
      `build-static.sh`):
      - copies the tree to `${PREFIX}.new` on the same filesystem, applies R2.1's libc choice
        there, and starts `${PREFIX}.new/bin/feldspar` with no arguments as a final check;
      - removes any earlier `${PREFIX}.old`, renames `${PREFIX}` to `${PREFIX}.old` and
        `${PREFIX}.new` to `${PREFIX}`; a leftover `${PREFIX}.new` from an interrupted run is
        deleted first;
      - carries over anything under `${PREFIX}` that an install did not put there, copying it
        into `${PREFIX}.new` before the swap and printing a warning that names each path. An
        install knows what it put there from a manifest it writes (`${PREFIX}/MANIFEST`, one path
        per line). A prefix without one (installed before this task) has its foreign files found
        by comparing against the old tarball layout: `bin/feldspar`, `ui/`, `plugins/`,
        `VARIANT`, `setup-host.sh`. A path the new tarball also ships is the new tarball's; the
        old file is not carried over. Check first that nothing writes into the prefix at run time;
        the bundled CPython must not (`.pyc` files are shipped, never written there).

      The curl installer (`scripts/install.sh`) and `--deploy` both use it:
      - Stop running `feldspar.service` and `feldspar@*.service` units before the swap and start
        the same ones after; `--no-restart` leaves running processes on the old tree, which the
        rename makes safe.
      - Read `${PREFIX}/VARIANT` and fetch the matching artifact: a `debian13-python` host keeps
        Python until R3.1 publishes the single artifact under that name.
      - Print the installed version before and after, the rollback command
        (`mv ${PREFIX} ${PREFIX}.bad && mv ${PREFIX}.old ${PREFIX}`), and that
        `docs/OPERATIONS.md` asks for a database dump before an upgrade.
      - The closing advice no longer says to restart after running it.

      Tests in the smoke test (R2.4), on `debian:13-slim` and `alpine:3`:
      - install a tarball, start the binary in the background, install a newer tarball over it:
        the install succeeds, the old process keeps running, and `${PREFIX}.old` holds the old
        tree;
      - a file only the old tarball had is gone from `${PREFIX}`;
      - a file an operator added (e.g. `${PREFIX}/local.conf`) is still there after the upgrade,
        and the output names it in a warning;
      - running the installer a second time with the same tarball changes nothing but `.old`.

## R3 — Release, documentation and retiring `static`

- [ ] R3.1 `--release` publishes the new artifact as `feldspar-linux-<arch>.tar.gz` (D13) and, for
      x86_64 and until R3.3, also as `feldspar-debian13-python.tar.gz` (D5). Upgrading a `debian13-python` host with the new
      tarball keeps its venv and its installed Python modules working (R1.6). Check this once by
      hand on a Debian 13 VM.
- [ ] R3.2 Documentation:
      - `docs/OPERATIONS.md` §1–§2: one download, the distribution list, which libc an install
        uses and why, and the CPython pin and how to bump it;
      - `README.md` §3–§4;
      - `docs/tutorial-python.md`: no Python packages to install on the host;
      - the header comments of `build-static.sh` and `static-build.Dockerfile`;
      - `docs/TECHNICAL_DESIGN.md` wherever it names the two artifacts.

      Tell the `~/feldspar-hosted` maintainers that its `ARCHITECTURE.md` lines 168 and 171 change
      (no `python3*` packages; a new tarball name, though the old one still works).
- [ ] R3.3 After one or two releases with both: stop building `static`. `feldspar.tar.gz` and
      `feldspar-debian13-python.tar.gz` both become copies of `feldspar-linux-x86_64.tar.gz`. Remove
      the `static` paths (`assert_static`, the `+crt-static` flags) from the scripts.
- [ ] R3.4 CHANGELOG entry: which distributions are supported, that Python needs nothing from the
      host, and the upgrade note for `debian13-python` hosts.

## R4 — aarch64 and macOS

- [ ] R4.1 `scripts/install.sh` picks the tarball by platform (D13): `uname -s` and `uname -m`
      (`x86_64`/`amd64`, `aarch64`/`arm64`) map to `feldspar-linux-<arch>.tar.gz` or
      `feldspar-macos-arm64.tar.gz`. Everything else gets D16's message for its platform and
      exits non-zero. `--force` no longer lets it continue on an unknown architecture, because
      there is nothing to download. On macOS: no package manager step, sudo for `/opt`, and no
      `.glibc` handling. R2.6's tree swap works unchanged. A test runs the script with `uname`
      stubbed for each case (Linux x86_64 and aarch64, armv7l, Darwin arm64 and x86_64,
      FreeBSD, MINGW64) and `--dry-run`, and checks the URL or the message.
- [ ] R4.2 Move the bucket upload, checksums and the post-build checks out of
      `build-static.sh` into `scripts/lib/release.sh` (D15), with no change in behaviour: a
      `--dry-run` release before and after prints the same commands.
- [ ] R4.3 aarch64 Linux (D12): a CI job on `ubuntu-24.04-arm` runs
      `build-static.sh --target aarch64-unknown-linux-gnu --release`. Check that R1.1's PBS
      download, R1.2's loader name and libraries, and `assert_python_linkage` handle aarch64.
      Run R2.4's smoke images on arm64 (every one except `archlinux`, which has no arm64 image;
      Arch Linux ARM is untested, say so in `docs/OPERATIONS.md`).
- [ ] R4.4 16 KB pages (D17): the build checks with `readelf -l` that every shipped ELF file's
      `LOAD` segments are aligned to at least 16 KB, on aarch64. If PBS's files are not, find out
      whether a PBS build option or a relink fixes it; if neither does, document that 16 KB-page
      kernels run without Python. Once by hand: the R1.7 self-check on a Raspberry Pi 5 with
      its default kernel, or on Asahi.
- [ ] R4.5 `scripts/build-macos.sh` (D15): build `sc-cli --features python` with
      `SC_BUNDLE_PREFIX=/opt/feldspar` against the pinned PBS `aarch64-apple-darwin` build, the
      UI bundles as on Linux, and the same tree layout (`bin/`, `python/`, `ui/`, `plugins/`,
      `install.sh`, `VARIANT` = `macos`, no `setup-host.sh`). Rewrite the `libpython`
      reference to `@loader_path/../python/lib/…` and sign ad hoc. Check with `otool -L` that
      nothing outside `/usr/lib`, `/System` and the bundle is linked, then run the R1.7
      self-check from an unpacked copy at `/opt/feldspar`. R1.4's home lookup works the same
      (`current_exe`).
- [ ] R4.6 A CI job on `macos-14` (Apple silicon) runs `build-macos.sh --release` and publishes
      `feldspar-macos-arm64.tar.gz` with its `.sha256`. Then `curl … | sh` on a clean Mac
      installs it, and `feldspar build-agent --once` reports the host ready without a Rust
      toolchain.
- [ ] R4.7 Documentation:
      - `docs/OPERATIONS.md`: a platform table (Linux x86_64 and aarch64, which distributions,
        macOS arm64 for development and build hosts, Windows through WSL2, what is not
        supported and why);
      - README §4.2: the download as the first way to get `feldspar` on a Mac, building from
        source as the second;
      - the build-host documentation: install with `curl … | sh` instead of building;
      - CHANGELOG entry.


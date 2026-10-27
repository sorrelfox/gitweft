#!/usr/bin/env python3
"""Release sources: the source tarball, the vendored crates, the license notices.

Usage: scripts/dist/sources.py OUT_DIR [--worktree]

Writes to OUT_DIR:
  gitweft-VERSION.tar.xz          the tracked files (HEAD's; the tree must be clean), or with
                                  --worktree the working tree's tracked and untracked,
                                  not ignored, files (for trying packaging changes); both
                                  without the paths .gitattributes marks export-ignore (the
                                  development repository's own files), and checked to hold
                                  what the package builds need (REQUIRED)
  gitweft-VERSION-vendor.tar.xz   extracts over the source tree: vendor/ (every crate of
                                  Cargo.lock, offline from cargo's cache), .cargo/config.toml
                                  (builds use vendor/), THIRD-PARTY-LICENSES.txt and
                                  .source-date-epoch
  gitweft-VERSION-bundled.inc     RPM macros: %bundled_license (the crates' licenses, SPDX)
                                  and %bundled_provides (Provides: bundled(crate(…)))
  THIRD-PARTY-LICENSES.txt        the notices, also for the DEB, AppImage and Flatpak

Crates that no x86_64 Linux build compiles (Windows, macOS, wasm …) stay in vendor/ as
stubs: their Cargo.toml and empty .rs files, so cargo still resolves Cargo.lock. Every file
gets the last commit's time and root ownership, and xz runs single-threaded: the same
commit gives the same bytes.
"""

import io
import json
import lzma
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
TARGET = "x86_64-unknown-linux-gnu"
# Left out of the source tarball: every path .gitattributes marks export-ignore (the files
# that belong to the development repository alone), as `git archive` leaves them out too.
# What the package builds take from the tarball (scripts/dist/build.sh, the RPM spec,
# deb.sh, appimage.sh, the Flatpak manifest): checked before the tarball is written.
REQUIRED = (
    "Cargo.toml",
    "Cargo.lock",
    "LICENSE",
    "crates/",
    "assets/fonts/",
    "locales/",
    "packaging/linux/",
    "packaging/rpm/gitweft.spec",
    "packaging/deb/control.in",
    "packaging/deb/copyright",
    "packaging/appimage/AppRun",
    "scripts/dist/build.sh",
    "scripts/dist/icons.sh",
    "scripts/dist/release-entry.sh",
    "assets/notices/syntaxes-and-themes.md",
)
LICENSE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE")

CARGO_CONFIG = """\
# Written by scripts/dist/sources.py: build from the vendored crates, never the network.
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"

[net]
offline = true
"""


def run(*args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True, **kwargs).stdout


def version():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    return manifest["workspace"]["package"]["version"]


def matches(path, pattern):
    """`path` is `pattern`, or inside it when it's a folder (ending in "/")."""
    return path.startswith(pattern) if pattern.endswith("/") else path == pattern


def excluded(files):
    """The paths among `files` that .gitattributes marks export-ignore."""
    if not files:
        return set()
    out = run("git", "check-attr", "--stdin", "-z", "export-ignore", input="\0".join(files) + "\0")
    fields = out.split("\0")  # path, attribute, value, repeated
    return {fields[i] for i in range(0, len(fields) - 2, 3) if fields[i + 2] == "set"}


def source_files(worktree):
    if worktree:
        listed = run("git", "ls-files", "-z", "--cached", "--others", "--exclude-standard")
        files = [p for p in listed.split("\0") if p and (ROOT / p).exists()]
    else:
        if run("git", "status", "--porcelain", "--untracked-files=no").strip():
            sys.exit("sources.py: the tree has uncommitted changes (commit them, or use --worktree)")
        files = [p for p in run("git", "ls-files", "-z").split("\0") if p]
    return sorted(set(files) - excluded(files))


def check_contents(files):
    """Stops when a build input is missing from the tarball's files; returns the top-level
    names, for the log."""
    missing = [r for r in REQUIRED if not any(matches(f, r) for f in files)]
    if missing:
        sys.exit(f"sources.py: the tarball would lack {missing}")
    return sorted({f.split("/")[0] + ("/" if "/" in f else "") for f in files})


def linux_packages():
    """(name, version) → package metadata, for every crates.io package an x86_64 Linux build uses."""
    meta = json.loads(run(
        "cargo", "metadata", "--offline", "--locked", "--format-version", "1",
        "--filter-platform", TARGET,
    ))
    used = {node["id"] for node in meta["resolve"]["nodes"]}
    return {
        (p["name"], p["version"]): p
        for p in meta["packages"]
        if p["id"] in used and p["source"]
    }


def vendor(into):
    run("cargo", "vendor", "--locked", "--offline", "--versioned-dirs", str(into))


def stub(crate):
    """Empties a crate no Linux build compiles, keeping what cargo needs to resolve it."""
    checksum = json.loads((crate / ".cargo-checksum.json").read_text())
    for path in sorted(crate.rglob("*"), reverse=True):
        if path.is_dir():
            if not any(path.iterdir()):
                path.rmdir()
        elif path.name == "Cargo.toml" and path.parent == crate:
            continue
        elif path.suffix == ".rs":
            path.write_bytes(b"")
        else:
            path.unlink()
    (crate / ".cargo-checksum.json").write_text(
        json.dumps({"files": {}, "package": checksum.get("package")})
    )


def license_files(crate):
    return sorted(
        f for f in crate.iterdir()
        if f.is_file() and f.name.upper().startswith(LICENSE_PREFIXES)
    )


def spdx(expression):
    """Old-style "MIT/Apache-2.0" as SPDX."""
    parts = [part.strip() for part in expression.replace(" / ", "/").split("/")]
    return " OR ".join(parts) if len(parts) > 1 else expression.strip()


def syntax_notices():
    """bat's syntax definitions and themes (compiled in through two-face) and their licenses,
    from assets/notices/syntaxes-and-themes.md, as plain text. gitweft-syntax's test keeps
    that file in step with the crate (#57)."""
    lines = []
    for line in (ROOT / "assets/notices/syntaxes-and-themes.md").read_text().splitlines():
        if line in ("<details>", "</details>") or line.startswith("````"):
            continue
        if line.startswith("<summary>") and line.endswith("</summary>"):
            lines.append(line[len("<summary>"):-len("</summary>")])
            continue
        lines.append(line)
    return lines


def notices(vendor_dir, packages):
    """The notices text: each crate's license and license files, identical texts once."""
    texts = {}  # text → its number
    lines = [
        "Third-party software in Gitweft",
        "===============================",
        "",
        "Gitweft (GPL-3.0-or-later) is built from the Rust crates below. Each entry names the",
        "crate, its version, its license (SPDX) and its license files; a text that several",
        "crates share is printed once, at the end, and referred to by number. Crates that ship",
        "no license file are under the standard text of their license, also at the end.",
        "The fonts' licenses (OFL-1.1) are installed next to this file, and the syntax",
        "definitions and themes compiled in have their own section at the end.",
        "",
    ]
    standard = {}  # SPDX id → an example file with its text
    fileless = set()
    for (name, ver), package in sorted(packages.items()):
        crate = vendor_dir / f"{name}-{ver}"
        expression = spdx(package["license"] or "see its license file")
        lines.append(f"{name} {ver} ({expression})")
        repo = package.get("repository")
        if repo:
            lines.append(f"  {repo}")
        files = license_files(crate)
        if not files:
            lines.append("  (no license file: the standard text of its license applies)")
            fileless.update(
                part.strip("() ") for part in expression.replace(" AND ", " OR ").split(" OR ")
            )
        for f in files:
            text = f.read_text(errors="replace").strip()
            number = texts.setdefault(text, len(texts) + 1)
            lines.append(f"  {f.name}: text {number}")
            upper = f.name.upper()
            for key, ident in (("APACHE", "Apache-2.0"), ("MIT", "MIT"), ("ZLIB", "Zlib"),
                               ("CC0", "CC0-1.0"), ("BSL", "BSL-1.0")):
                if key in upper:
                    standard.setdefault(ident, number)
        lines.append("")
    lines += ["", "License texts", "=============", ""]
    for text, number in texts.items():
        lines += [f"--- text {number} ---", text, ""]
    missing = sorted(i for i in fileless if i not in standard)
    lines += ["", "Standard texts for crates without license files", ""]
    for ident in sorted(i for i in fileless if i in standard):
        lines.append(f"{ident}: text {standard[ident]} (with that crate's authors as the copyright holders)")
    for ident in missing:
        lines.append(f"{ident}: see https://spdx.org/licenses/{ident}.html")
    lines += [
        "",
        "Syntax definitions and themes",
        "=============================",
        "",
        "Gitweft highlights code with bat's collection of syntax definitions and themes,",
        "compiled in through the two-face crate. Each carries its own license; below are the",
        "acknowledgements two-face ships, as plain text.",
        "",
    ]
    lines += syntax_notices()
    return "\n".join(lines) + "\n"


def rpm_macros(packages):
    expressions = sorted({spdx(p["license"]) for p in packages.values() if p["license"]})
    joined = " AND ".join(f"({e})" if " " in e else e for e in expressions)
    provides = [
        f"bundled(crate({name})) = {ver.replace('-', '~')}" for name, ver in sorted(packages)
    ]
    # rpm cuts a macro file's line at 16 KiB and takes one preamble line per macro, so the
    # spec prints one `Provides:` per chunk (a Lua loop over %bundled_provides_1 … _count).
    chunks = [provides[at:at + 100] for at in range(0, len(provides), 100)]
    lines = [
        "# Written by scripts/dist/sources.py for packaging/rpm/gitweft.spec.",
        f"%bundled_license {joined}",
        f"%bundled_provides_count {len(chunks)}",
    ]
    lines += [f"%bundled_provides_{i} {', '.join(chunk)}" for i, chunk in enumerate(chunks, 1)]
    return "\n".join(lines) + "\n"


def tar_xz(out, prefix, entries, epoch):
    """A reproducible tar.xz: `entries` are (path in the archive, file on disk or bytes)."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, source in sorted(entries, key=lambda e: e[0]):
            info = tarfile.TarInfo(f"{prefix}/{name}")
            info.mtime, info.uid, info.gid, info.uname, info.gname = epoch, 0, 0, "", ""
            if isinstance(source, bytes):
                info.size, info.mode = len(source), 0o644
                tar.addfile(info, io.BytesIO(source))
            elif source.is_symlink():
                info.type, info.linkname, info.mode = tarfile.SYMTYPE, str(source.readlink()), 0o777
                tar.addfile(info)
            else:
                data = source.read_bytes()
                info.size = len(data)
                info.mode = 0o755 if source.stat().st_mode & 0o111 else 0o644
                tar.addfile(info, io.BytesIO(data))
    out.write_bytes(lzma.compress(buffer.getvalue(), preset=6))


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out = pathlib.Path(sys.argv[1]).resolve()
    worktree = "--worktree" in sys.argv[2:]
    out.mkdir(parents=True, exist_ok=True)
    ver = version()
    prefix = f"gitweft-{ver}"
    epoch = int(run("git", "log", "-1", "--format=%ct").strip())

    files = source_files(worktree)
    top = check_contents(files)
    tar_xz(out / f"{prefix}.tar.xz", prefix, [(f, ROOT / f) for f in files], epoch)
    print(f"{prefix}.tar.xz holds {len(files)} files: {' '.join(top)}")

    vendor_dir = out / "vendor"
    shutil.rmtree(vendor_dir, ignore_errors=True)
    vendor(vendor_dir)
    packages = linux_packages()
    # Folder names, not a split on '-': versions can have one ("0.4.0+sdk-1.4").
    built = {f"{name}-{ver}" for name, ver in packages}
    for crate in vendor_dir.iterdir():
        if crate.name not in built:
            stub(crate)
    text = notices(vendor_dir, packages)
    (out / "THIRD-PARTY-LICENSES.txt").write_text(text)
    (out / f"{prefix}-bundled.inc").write_text(rpm_macros(packages))

    entries = [
        (str(p.relative_to(out)), p)
        for p in vendor_dir.rglob("*")
        if p.is_file() or p.is_symlink()
    ]
    entries += [
        (".cargo/config.toml", CARGO_CONFIG.encode()),
        ("THIRD-PARTY-LICENSES.txt", text.encode()),
        (".source-date-epoch", f"{epoch}\n".encode()),
    ]
    tar_xz(out / f"{prefix}-vendor.tar.xz", prefix, entries, epoch)
    shutil.rmtree(vendor_dir)
    for name in (f"{prefix}.tar.xz", f"{prefix}-vendor.tar.xz"):
        size = (out / name).stat().st_size / 2**20
        print(f"{out / name}  {size:.1f} MiB")
    print(f"{len(packages)} crates built on Linux; notices {len(text) // 1024} KiB")


if __name__ == "__main__":
    main()

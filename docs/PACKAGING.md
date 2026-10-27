# Building the packages

How the RPM, DEB, AppImage and Flatpak are made, how to build them on your machine with one
command each, and how a release is cut. If you only want the binary, `just build dist` is
enough; this page is about the packages.

## The idea

Every package is built from the same two files:

- `gitweft-VERSION.tar.xz`, the source tree as committed, and
- `gitweft-VERSION-vendor.tar.xz`, every Rust crate the build needs, taken from cargo's cache,
  plus a `.cargo/config.toml` that points cargo at them and switches the network off.

With both unpacked over each other, `scripts/dist/build.sh` runs `cargo build --frozen --profile
dist`, which cannot download anything. Each package format has one script in `scripts/dist/`
that runs this build inside a container of the right distribution, packs the result, installs
the package in a fresh container (the AppImage is started in a bare one instead), and starts the
installed app with `--network none` to prove that it loads a repository offline. `just release`
runs all of them in turn, and the release workflow on GitHub runs the same scripts.

The toolchains differ because each channel requires it: the RPM is built with Fedora's own Rust
(COPR would build the same SRPM the same way), the Flatpak with the Freedesktop SDK's Rust
extension, the DEB with the Rust named in `rust-toolchain.toml`. The DEB and the AppImage share
one binary: the AppImage repacks the DEB's, which is built on Ubuntu 24.04 so that it runs on
anything newer. The AppImage also carries the five libraries the binary needs that not every
distribution installs (xkbcommon, xkbcommon-x11 and xcb-xkb for the keyboard, the Vulkan loader,
and wayland-egl for the OpenGL fallback), taken from the same Ubuntu; the libraries every desktop
has, such as libxcb, libwayland-client, EGL and glibc, come from the host, as the AppImage
project's excludelist prescribes. Within one toolchain the build is reproducible. `just
repro-check` builds twice, in two folders at different paths (one with a space in it), and
compares the binaries byte for byte.

## What you need

| For | On your machine |
|---|---|
| The sources | python3, git, and the Rust toolchain (`cargo fetch` fills the cache the vendor tarball is made from) |
| RPM, DEB | podman, or docker with `CONTAINER=docker` |
| AppImage | squashfs-tools, curl and podman, plus the DEB's binary and libraries, which are built for you when they are missing |
| Flatpak | flatpak and flatpak-builder; the SDK is installed for your user from Flathub on the first run |
| `just release` | all of the above, plus desktop-file-utils and appstream for the metadata check |

The containers download their own build dependencies with dnf or apt, and the DEB build installs
the pinned Rust with rustup when its image has none. Each build needs about 10 GB of free
memory for its last step, the link-time optimisation of the whole program. The spec keeps
Fedora's build flags from adding full debug information on top of that (`rustflags_debuginfo`):
with it, the RPM's link needed more than the 16 GB of a GitHub runner and was killed.

## The commands

| Command | Result | Where |
|---|---|---|
| `just dist-src` | the two tarballs, `THIRD-PARTY-LICENSES.txt`, the RPM's bundled-crates macros | `target/dist-src/` |
| `just rpm` | the RPM and SRPM, built in Fedora 44 as COPR does, installed and smoke-tested | `target/rpm/` |
| `just deb` | the DEB, built in Ubuntu 24.04, installed and smoke-tested on Debian 13, Ubuntu 24.04 and 26.04, checked with lintian | `target/deb/` |
| `just appimage` | the AppImage, from the DEB's binary and libraries, started in a bare Debian 13 container and smoke-tested there offline | `target/appimage/` |
| `just flatpak` | the Flatpak bundle, built in the Freedesktop SDK and smoke-tested in the sandbox | `target/flatpak/` |
| `just release` | all of the above, plus `SHA256SUMS` | `target/release/VERSION/` |
| `just repro-check` | two builds, compared | |

Each command prints the path of the finished file at the end. The tarballs are made from the
last commit; `just rpm --worktree` (and the same for the others) takes the working tree instead,
to try a packaging change before committing it. `just appimage` reuses the DEB's binary when one
is there; run `just deb` first to rebuild it.

### x86_64 and aarch64

The RPM, the DEB and the AppImage are built for x86_64 and for aarch64 (64-bit ARM). The
scripts build for the machine's own architecture, or for `GITWEFT_ARCH` (`x86_64` or `aarch64`)
when it is set; their containers then run as that architecture. On an x86_64 machine that needs
QEMU user-mode emulation (`sudo dnf install qemu-user-static` on Fedora), and the build is
several times slower. `just release --aarch64`, after a `just release`, builds and tests the
aarch64 packages and adds them to `target/release/VERSION/`. The release workflow builds each
architecture natively, the aarch64 packages on GitHub's arm64 runners.

The vendor tarball holds the crates of both builds (aarch64 needs a few fewer), so one pair of
tarballs serves every package. The Flatpak bundle is built for x86_64 only; Flathub builds its
own for both architectures.

## The scripts

| Script | What it does |
|---|---|
| `sources.py` | Writes the source tarball (the tracked files; `--worktree` takes the working tree) and the vendor tarball. Crates that no Linux build compiles (Windows, macOS, wasm; about 250 of them) are reduced to stubs, so the vendor tarball is about 45 MiB rather than a gigabyte. Also writes `THIRD-PARTY-LICENSES.txt`, the license notice for every crate the Linux build uses, and the RPM macros that declare them as bundled. |
| `build.sh` | The release build from an unpacked tree: `cargo build --frozen --profile dist` with a private `CARGO_HOME`, `SOURCE_DATE_EPOCH` from the sources, and the tree's path remapped to `/usr/src/gitweft`, so the binary does not depend on where it was built. |
| `rpm.sh` | In a Fedora 44 container: makes the SRPM from `packaging/rpm/gitweft.spec`, installs its build dependencies with `dnf builddep`, rebuilds it as a normal user, installs the RPM, then runs the smoke test in a container with no network. |
| `deb.sh` | In an Ubuntu 24.04 container: builds, strips, computes `Depends` with `dpkg-shlibdeps` plus the libraries the app loads at run time (the Vulkan loader, Wayland, EGL), writes the changelog from the release entry, packs with `dpkg-deb`. Leaves the binary, the icons and the libraries the AppImage bundles (with their copyright files) next to the DEB; the AppImage's copy of the binary gets a run path, `$ORIGIN/../lib`, with patchelf. |
| `test-deb.sh`, `lint-deb.sh` | Install the DEB on Debian 13, Ubuntu 24.04 and 26.04 and run the smoke test offline; run lintian with errors fatal. |
| `appimage.sh` | The DEB's binary, desktop file, metainfo, icons, the bundled libraries in `usr/lib` and the licenses in a zstd squashfs behind the AppImage type-2 runtime, which is pinned by tag and checksum and downloaded once into `target/tools/`. git comes from the host. |
| `test-appimage.sh` | Starts the AppImage in a Debian 13 container that has only libxcb1 and git installed, prints where each library came from (the bundle or the host), and runs the smoke test offline. A library the bundle forgot fails the start here, as it would on a user's machine. |
| `flatpak.sh` | Builds `packaging/flatpak/io.github.sorrelfox.Gitweft.yml` with flatpak-builder, bundles it, installs the bundle for the user and runs the smoke test with `flatpak run --unshare=network`. The SDK's git is copied into the app, because the Freedesktop Platform has none. |
| `flathub.sh` | Writes Flathub's copy of the Flatpak manifest: the same file, with the release's two tarballs as URLs on the GitHub Release and their SHA-256 in place of the local paths, and `flathub.json`, which names the architectures Flathub builds: x86_64 and aarch64, the two whose crates the vendor tarball holds. |
| `release-notes.sh` | Prints a version's entry in `CHANGELOG.md`, the notes of its GitHub Release. |
| `icons.sh` | Installs the SVG icons and renders PNGs from 16 to 512 px with rsvg-convert. |
| `manpage.sh` | Writes `gitweft(1)` from the binary's own `--help`, so the options cannot drift. |
| `validate-metadata.sh` | `desktop-file-validate` and `appstreamcli validate --pedantic` on the desktop file and the metainfo. |
| `check-version.sh` | Every file that carries the version agrees with `Cargo.toml` (see "Making a release"). This is `just versions`. |
| `release-entry.sh` | Prints the newest release's date and note from the metainfo, the one source the RPM and DEB changelogs follow. |
| `arch.sh` | Which architecture the other scripts build for (`GITWEFT_ARCH`, else the machine's), and the names the packages give it. |
| `pins.env`, `update-pins.sh`, `flatpak-pins.sh`, `rustup-init.sh` | The pinned inputs, next. |

## Pinned inputs

What a build starts from is pinned in `scripts/dist/pins.env`, so that rebuilding a version later
starts from the same inputs:

- the container images (Fedora 44, Ubuntu 24.04 and 26.04, Debian 13) by digest;
- rustup by version, and `rustup-init` by its SHA-256 for each architecture, instead of piping
  `sh.rustup.rs` into a shell;
- the Flatpak SDK, Platform and Rust extension by commit.

`scripts/dist/update-pins.sh` fetches the current digests and commits and rewrites the file; read
the diff, rebuild the packages, and commit. `GITWEFT_RPM_IMAGE`, `GITWEFT_DEB_IMAGE` and
`GITWEFT_DEB_TEST_IMAGES` point the scripts at other images (a local mirror, say) without
touching the pins.

The Flatpak build needs the SDK, the Platform and the Rust extension installed for your user at
the pinned commits. `flatpak-pins.sh` installs them from Flathub when they are missing. When one
is installed at a different commit, it stops and tells you, because moving it may affect other
Flatpak apps you use; `GITWEFT_FLATPAK_DEPS=flathub just flatpak` says that is fine.

## Making a release

1. Pick the version. Set it in `Cargo.toml` (`[workspace.package]`) and in the spec's
   `Version:` line.
2. Write the release note once, in three places that must agree: a new `<release>` at the top
   of `packaging/linux/io.github.sorrelfox.Gitweft.metainfo.xml` (with its date and a `<p>`), a
   new `%changelog` entry at the top of `packaging/rpm/gitweft.spec` (same day, same note), and a
   new entry at the top of `CHANGELOG.md` (`## VERSION (DATE)`, then the same note as its first
   paragraph). `just versions` checks all of it; it is also the first step of `just check` and
   of `just release`. The Flatpak manifest and the DEB's `control.in` take the version from
   `Cargo.toml` themselves.
3. Run `just check` and `just release`, and try the packages.
4. Commit, then push a tag `vVERSION`. The release workflow (`.github/workflows/release.yml`)
   checks that the tag matches `Cargo.toml`, builds and tests every package with the same
   scripts, checks that the build is reproducible, attests every package's build provenance
   (`actions/attest-build-provenance`, checked with `gh attestation verify FILE --repo
   sorrelfox/gitweft`), and creates a **draft** GitHub Release with the files, `SHA256SUMS` and
   the version's `CHANGELOG.md` entry as its notes. Read it over, then publish it. The run also
   keeps a `flathub` artifact with the version's Flathub files (see "Flathub"). "Run workflow"
   on the Actions page builds and tests without releasing anything.

### COPR

Fedora's COPR can build the RPM from the SRPM. You need a Fedora account and `copr-cli` with its
token in `~/.config/copr`:

```sh
copr-cli create gitweft --chroot fedora-44-x86_64 --chroot fedora-rawhide-x86_64   # once
just rpm && copr-cli build gitweft target/rpm/gitweft-*.src.rpm
```

COPR builds the SRPM in mock with the chroot's own Rust; `rpm.sh` is the same build in a
container.

### Flathub

Flathub builds the app on its own servers, from a manifest in its repository for the app
(`flathub/io.github.sorrelfox.Gitweft`). That manifest is `flathub.sh`'s output: the same build
as `just flatpak`, from the release's tarballs on GitHub. The release workflow writes it for
each tag, in the run's `flathub` artifact; `just flathub DIR` writes it from tarballs you have.

- **The first time:** follow Flathub's submission guide. The pull request adds the two files
  of the `flathub` artifact, `io.github.sorrelfox.Gitweft.yml` and `flathub.json`. Two
  permissions need a word in it: `--filesystem=home`, because repositories live anywhere in
  the home folder, and `--talk-name=org.freedesktop.Flatpak`, because the terminal, file
  manager, editor and diff tools the user chose run on the host, as do git's credential
  helpers and hooks.
- **Each release after that:** once the GitHub Release is published, replace the two files in
  the app's Flathub repository with the new run's `flathub` artifact, in a pull request there.
  Flathub's own build checks it; merging publishes the update.

## When something goes wrong

- **`unshare: Operation not permitted` during `just check`**: the system forbids unprivileged
  user namespaces (Ubuntu's AppArmor does by default). `sudo sysctl -w
  kernel.apparmor_restrict_unprivileged_userns=0` allows them, or run the smoke test without the
  namespace; the socket audit is the check that matters.
- **A build is killed in its last minutes**: the final link ran out of memory. 10 GB free is
  enough; close what else you can and run it again.
- **`flatpak-pins.sh: ... is at <commit>, pins.env pins <commit>`**: your installed SDK moved past
  the pin (a `flatpak update` does that). `GITWEFT_FLATPAK_DEPS=flathub just flatpak` moves it
  back for the build.
- **The AppImage says `error while loading shared libraries`**: the binary needs a library that
  is neither bundled nor on the host. `test-appimage.sh` shows where each library comes from;
  add the missing one to the list in `deb.sh`, unless the AppImage excludelist says every host
  has it.
- **`sources.py: the tree has uncommitted changes`**: commit them, or add `--worktree`.
- **A container image cannot be pulled**: the pinned digest is looked up at the registry;
  `GITWEFT_*_IMAGE` points at a mirror, and `update-pins.sh` refreshes the pins.

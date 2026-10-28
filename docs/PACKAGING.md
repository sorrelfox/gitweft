# Building the packages

How the RPM, DEB, AppImage and Flatpak are made, and the macOS disk image and the Windows
installer, how to build them on your machine with one command each, and how a release is cut.
If you only want the binary, `just build dist` is enough; this page is about the packages.

## The idea

Every Linux package is built from the same two files:

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
| `just macos` | a Mac with Xcode, rustup and Python 3.11 or later (see "macOS") |
| `just windows` | Windows with Git for Windows, rustup, the Visual Studio Build Tools, Python 3.11 or later and Inno Setup 6.3 or later (see "Windows") |

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
| `just macos` | the app in a disk image, one universal binary for Apple silicon and Intel, smoke-tested from the image | `target/macos/` |
| `just windows` | the installer and the zip, installed, smoke-tested and uninstalled | `target/windows/` |

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
| `macos.sh`, `test-macos.sh` | The macOS app and disk image, and its test (see "macOS"). |
| `windows.sh`, `test-windows.sh` | The Windows installer and zip, and their test (see "Windows"). |
| `notices.py` | `THIRD-PARTY-LICENSES.txt` for the crates of a macOS or Windows build, the same text `sources.py` writes for Linux, with the license files read from cargo's cache. |
| `pins.env`, `update-pins.sh`, `flatpak-pins.sh`, `rustup-init.sh` | The pinned inputs (see "Pinned inputs"). |

## macOS

`just macos`, on a Mac, makes `target/macos/Gitweft-VERSION-macos-universal.dmg`: a disk image
with `Gitweft.app` and a link to the Applications folder to drag it onto. `scripts/dist/macos.sh`
does the work:

- It builds the binary with the `dist` profile twice, for Apple silicon (`aarch64-apple-darwin`)
  and for Intel (`x86_64-apple-darwin`), and joins the two with `lipo` into one universal
  binary. One download that runs on every Mac is worth the second build and the bigger file.
- The app's `Info.plist` is `packaging/macos/Info.plist` with the version from `Cargo.toml`
  filled in. The bundle id is `io.github.sorrelfox.Gitweft`, the Linux app id, and folders are
  declared as a document type, so that a folder can be dropped on the app's icon.
- The icon is made during the build: resvg renders the scalable SVG of `packaging/linux/icons`
  at each size of an icon set, and `iconutil` turns the set into `Gitweft.icns`. resvg is
  built once, with `cargo install` at the version in `pins.env`, into `target/tools/`.
- `notices.py` writes `THIRD-PARTY-LICENSES.txt` for the crates of the macOS build. It goes
  into `Contents/Resources`, next to the GPL and the fonts' licenses.
- The oldest macOS the app runs on is 10.15.7 (Catalina): GPUI compiles its Metal shaders for
  that version, and the script sets `MACOSX_DEPLOYMENT_TARGET` to match. Macs with Apple
  silicon have macOS 11 or later anyway.

Unlike the Linux packages, the macOS and Windows builds start from the checkout, not from the
release tarballs: the vendor tarball holds only stubs of the crates those systems need.
`cargo fetch --locked` downloads the crates once, checked against `Cargo.lock`, and the builds
then run with `--frozen`, offline.

What you need: Xcode, rustup and Python 3.11 or later. Xcode itself, not only the Command
Line Tools, because GPUI's build compiles its Metal shaders with `xcrun metal`, which only
Xcode has; recent versions of Xcode download it separately (`xcodebuild -downloadComponent
MetalToolchain`). rustup adds the two targets itself. The Command Line Tools' `python3` is too
old for `notices.py`; Homebrew's is fine.

`test-macos.sh` mounts the image, checks the app's layout and signature, and runs `--version`
and the smoke test on a fresh repository from the image, with the network denied by
`sandbox-exec`: on a Mac with Apple silicon once natively and once under Rosetta as Intel,
when Rosetta is installed, so that both halves of the binary are tried.

### Signing and notarisation

Without anything set up, the app is signed ad hoc (`codesign --sign -`). That is enough for
Apple silicon, which runs no unsigned program at all, but Gatekeeper does not trust it: the
first time the downloaded app is opened, macOS refuses, and the user has to allow it once, in
System Settings, Privacy & Security, "Open Anyway" (on macOS 14 and older, also with Open in
the app's context menu in Finder).

A signature Gatekeeper accepts needs a membership of the Apple Developer Program and its
"Developer ID Application" certificate in your keychain. Then

```sh
GITWEFT_MACOS_SIGN_IDENTITY="Developer ID Application: NAME (TEAM ID)" just macos
```

signs the app, with the hardened runtime, and the disk image with it. The hardened runtime
blocks Apple events unless the app is entitled to send them, and Gitweft sends one when it
asks Terminal to run a command (through `osascript`, when no terminal is set in the
settings), so the app is signed with `packaging/macos/Gitweft.entitlements`, which allows
them. The first time, macOS asks the user whether Gitweft may control Terminal, with the
reason in `Info.plist`'s `NSAppleEventsUsageDescription`. When notarytool has
credentials as well, both are notarised by Apple and the tickets stapled to them: either a
keychain profile, saved once with `xcrun notarytool store-credentials gitweft` and named with
`GITWEFT_MACOS_NOTARY_PROFILE=gitweft`, or `GITWEFT_MACOS_NOTARY_APPLE_ID`,
`GITWEFT_MACOS_NOTARY_TEAM_ID` and `GITWEFT_MACOS_NOTARY_PASSWORD` (an app-specific password
of that Apple ID). The app is notarised on its own first, so that it carries its own ticket
and opens without a network connection once copied out of the image.

The release workflow does the same with the repository's secrets: `GITWEFT_MACOS_CERTIFICATE`
(the certificate with its private key, exported from Keychain Access as a .p12 file, in
base64), `GITWEFT_MACOS_CERTIFICATE_PASSWORD` (the .p12 file's password),
`GITWEFT_MACOS_SIGN_IDENTITY`, and the three `GITWEFT_MACOS_NOTARY_*` above. Without them it
signs ad hoc.

### git on macOS

Gitweft runs the `git` it finds on `PATH`. An app started from Finder or the Dock does not
get the `PATH` of your shell, only the system's: `/usr/bin:/bin:/usr/sbin:/sbin`. There,
`/usr/bin/git` is Apple's: it runs the git of the Xcode Command Line Tools (or of Xcode), and
offers to install the Command Line Tools when neither is there. Its version (2.39 or later
in the Command Line Tools of recent years) is new enough. A git from Homebrew (in `/opt/homebrew/bin`, or
`/usr/local/bin` on Intel) or MacPorts is not on that `PATH`, so the app uses Apple's git even
when another one is installed. Started from a terminal as
`/Applications/Gitweft.app/Contents/MacOS/gitweft`, it gets the shell's `PATH` and its git.

## Windows

`just windows`, on Windows, in Git Bash, makes two files in `target/windows`:
`Gitweft-VERSION-windows-x86_64-setup.exe`, an installer, and
`Gitweft-VERSION-windows-x86_64.zip`, the same files to unpack anywhere. `scripts/dist/windows.sh`

- builds the binary with the `dist` profile for `x86_64-pc-windows-msvc`, and its console
  twin (see below). The executable carries its icon and version details
  (`crates/gitweft/build.rs` embeds `crates/gitweft/resources/windows/gitweft.ico`), and the
  installer uses the same icon;
- puts `gitweft.exe`, `gitweft.com`, the GPL as `LICENSE.txt`, the fonts' licenses and
  `THIRD-PARTY-LICENSES.txt` (`notices.py`'s, for the crates of the Windows build) in one
  folder, and zips it with Windows' own `tar.exe`;
- compiles `packaging/windows/gitweft.iss` with Inno Setup, with the version from `Cargo.toml`.

The installer installs for the current user by default, into
`%LOCALAPPDATA%\Programs\Gitweft`, and needs no administrator rights; its first page offers an
installation for all users instead. It adds a Start menu entry, a desktop icon if you tick it,
and an uninstaller in the list of installed apps. `/VERYSILENT /CURRENTUSER` (or `/ALLUSERS`)
installs without any questions.

There is no build for Windows on ARM: those machines run the x86_64 build through Windows'
own emulation.

`test-windows.sh` runs the installer without questions into a temporary folder, checks the
Start menu entry, runs `--version` and the smoke test with the installed app, checks that
`gitweft.com` prints the same version and passes on an exit code, uninstalls it and checks
that nothing is left; then it does the same from the unpacked zip. Where Gitweft is installed
already, it skips the installer, which would replace that installation. Unlike on Linux and
macOS, the smoke test runs with the network available: Windows has no simple way to take it
away from one program.

What you need: Git for Windows (its bash runs the scripts), rustup with the MSVC toolchain,
the Visual Studio Build Tools with "Desktop development with C++" (the linker and the Windows
SDK), Python 3.11 or later, and Inno Setup 6.3 or later. GitHub's Windows runners have all of
it; if a runner ever lacks Inno Setup, the release workflow installs the version in `pins.env`
with Chocolatey.

### gitweft.com

The release `gitweft.exe` is a GUI program, so starting it from Explorer or a shortcut opens
no console window. But cmd and PowerShell don't wait for a GUI program: `gitweft.exe --version`
would print after the prompt is back, and its exit code would be lost. So next to it there is
`gitweft.com`, a small console program (`crates/gitweft-console`, nothing but std) that runs
`gitweft.exe` from its own folder with the same arguments, waits for it, and returns its exit
code. For a bare `gitweft`, cmd and PowerShell try `.com` before `.exe` (the order in
`PATHEXT`), so they run the twin; Explorer, the shortcuts and git, which runs Gitweft as a
helper, use the full path of `gitweft.exe`. The twin has the same icon and version details
(its own `build.rs`). Like in a Linux terminal, `gitweft PATH` then waits until the window is
closed, unless a running Gitweft takes the path; `gitweft.exe PATH` returns at once.

The installer does not add its folder to `PATH`. To type `gitweft` anywhere, add
`%LOCALAPPDATA%\Programs\Gitweft` (or the folder you chose) to your `PATH`.

### Signing

Unless you give a code-signing certificate, nothing is signed, and Windows SmartScreen warns
when the installer or the program is started ("Windows protected your PC": "More info", then
"Run anyway") until the files have gathered a reputation. A signature needs a code-signing
certificate from a certificate authority. With one in a .pfx file,

```sh
GITWEFT_WINDOWS_CERTIFICATE=path/to/certificate.pfx GITWEFT_WINDOWS_CERTIFICATE_PASSWORD=... just windows
```

signs `gitweft.exe`, `gitweft.com` and the installer with signtool, from the Windows SDK, with
a timestamp (`GITWEFT_WINDOWS_TIMESTAMP_URL` picks the server; DigiCert's by default). The
uninstaller that Inno Setup writes stays unsigned. Certificates issued since June 2023 live on
a hardware token or in a cloud signing service rather than in a .pfx file; with one of those,
the `sign` function in `windows.sh` is the one place to change. The release workflow takes the .pfx file
in base64 and its password from the secrets `GITWEFT_WINDOWS_CERTIFICATE` and
`GITWEFT_WINDOWS_CERTIFICATE_PASSWORD`.

### git on Windows

Gitweft runs the `git` it finds on `PATH`. Git for Windows puts `C:\Program Files\Git\cmd` on
`PATH` with its recommended choice, "Git from the command line and also from 3rd-party
software"; with "Use Git from Git Bash only", the app finds no git.

## Pinned inputs

What a build starts from is pinned in `scripts/dist/pins.env`, so that rebuilding a version later
starts from the same inputs:

- the container images (Fedora 44, Ubuntu 24.04 and 26.04, Debian 13) by digest;
- rustup by version, and `rustup-init` by its SHA-256 for each architecture, instead of piping
  `sh.rustup.rs` into a shell;
- the Flatpak SDK, Platform and Rust extension by commit;
- resvg, which renders the macOS icon, by version (cargo checks the crate against the
  registry's checksum and builds it with its own `Cargo.lock`);
- Inno Setup by version, used only when a Windows runner has none (Chocolatey checks the
  download against the package's checksum).

`scripts/dist/update-pins.sh` fetches the current digests and commits and rewrites the file; read
the diff, rebuild the packages, and commit. It keeps the resvg and Inno Setup versions as they
are; change those by hand. `GITWEFT_RPM_IMAGE`, `GITWEFT_DEB_IMAGE` and
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
3. Run `just check` and `just release`, and try the packages. `just macos` and `just windows`
   need those systems; without them, the release workflow is where those packages are built.
4. Commit, then push a tag `vVERSION`. The release workflow (`.github/workflows/release.yml`)
   checks that the tag matches `Cargo.toml`, builds and tests every package with the same
   scripts (the macOS and Windows ones on GitHub's macOS and Windows runners, signed when the
   repository has the secrets described above), checks that the build is reproducible, attests every package's build provenance
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

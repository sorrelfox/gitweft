# Offline, and what stays on your machine

Gitweft works without an internet connection, and it never contacts a server on its own. This
page says exactly what that means, how it is enforced in the code and the build, and how you can
check it yourself.

## The promise

1. **The Gitweft process opens no internet connections.** It creates no AF_INET or AF_INET6
   sockets. It does use Unix-domain sockets, which stay on your machine: for Wayland or X11, for
   D-Bus (desktop portals), and for two of its own, the credential-prompt relay and the
   single-instance socket that lets `gitweft PATH` open a tab in the running window. Both live
   in `$XDG_RUNTIME_DIR/gitweft/`, readable by you alone.
2. **All network traffic comes from git, ssh or git-lfs**, started by Gitweft as child
   processes, and goes only to the remotes you configured. They run only when you ask: fetch,
   pull, push, clone, a submodule update, an LFS operation, or background fetch, which is off by
   default and switched on per repository.
3. **There is no** telemetry, analytics, crash reporting, update check, remote font, avatar,
   remote image or news feed. Not switched off: not there.
4. **Everything the app needs is inside the binary**: fonts, icons, themes, syntax definitions.
   Nothing is downloaded at run time.
5. **Gitweft stores no credentials.** Password and passphrase prompts are shown in the app and
   handed to git; your git credential helper and ssh-agent do the remembering.

## How it is enforced

A promise in a README is cheap. These are the checks that make it hard to break by accident,
from the dependency list down to the running process:

| Layer | Check |
|---|---|
| Dependencies | `deny.toml` (run by `cargo deny check`) bans HTTP clients, TLS stacks, WebSocket and telemetry crates, and gix's network features. The only allowed source of crates is crates.io. |
| Code | `clippy.toml` forbids `std::net` connects and binds, registering an HTTP client with GPUI, and starting processes outside the git runner. `scripts/url-audit.sh` fails on any URL literal in the Rust sources, tests and comments aside; the only exceptions are the two Git LFS pointer-format identifiers, which nothing fetches. |
| The running app | `just check` runs `gitweft --headless-smoke` on a repository inside a namespace that has no network at all (`unshare -rn`), and it must succeed. It then runs the same under `strace -f -e trace=socket,connect` and fails if any process opened an internet socket (`scripts/socket-audit.sh`, which first proves to itself that it would notice one). |
| The build | Release builds use vendored crates and `cargo build --frozen` with `net.offline`, so a build cannot download anything. Each package is built in a container, then installed and started in a container with `--network none`. Packaging tools only ever download at build time (the containers' build dependencies, the pinned AppImage runtime checked against its SHA-256); the app never does. |
| CI | The same `just check` runs on every push. |

## Check it yourself

```sh
# Load a repository with no network available at all
unshare -rn gitweft --headless-smoke /path/to/repo

# Trace every socket the app and its children open; the grep should print nothing
strace -f -e trace=socket,connect gitweft --headless-smoke /path/to/repo 2>&1 \
    | grep -E 'AF_INET6?' || echo "no internet sockets"
```

From a checkout, `just socket-audit /path/to/repo` does the second one on a fresh build.

And in the app itself, the Activity button in the title bar lists every git command Gitweft ran,
with its arguments, exit code and output. If git talked to a remote, it is there.

## Two things worth knowing

**Signature checks.** The commit details show whether a commit's signature checks out. Git
verifies it locally with `gpg` or `ssh-keygen`, using your keyring or your
`gpg.ssh.allowedSignersFile`. gpg fetches a missing key from a keyserver only if you turned on its
`auto-key-retrieve` option yourself; that is your gpg configuration, not something Gitweft asks
for.

**Partial clones.** In a clone made with `--filter=blob:none`, git normally fetches a missing
file's content from the remote the moment something reads it. Gitweft's read-only git calls turn
that off (`GIT_NO_LAZY_FETCH=1`, honoured by git 2.44 and newer), and gix never fetches. So a
diff whose content is not there says so and offers a **Fetch missing contents** button; only
that downloads, and only the versions needed. The same goes for a file shown at an old commit
and for blame. A commit's file list then shows no line counts and finds renames by identical
content only, instead of downloading the old versions to compare them.

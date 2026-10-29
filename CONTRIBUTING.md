# Contributing to Gitweft

Thanks for taking a look. Gitweft is a one-person project, so this file is written for someone
arriving fresh: how to build and test it, how the code is laid out, and the handful of rules that
keep it working the way it does. If something here is unclear, that is a bug in this file; please
say so in an issue.

## Reporting a bug

Open an issue at https://github.com/sorrelfox/gitweft/issues with:

- what you did, what you expected, and what happened instead;
- the output of `gitweft --version` and `git --version`, and your distribution and desktop;
- the Activity log, if git was involved. The Activity button in the title bar lists every git
  command Gitweft ran, with its arguments, exit code and output; copy the entries that matter.

If the app crashed, whatever it printed to the terminal it was started from helps a lot.

## Setting up

You need the Rust toolchain named in `rust-toolchain.toml` (rustup installs it on first use), a
C compiler, pkg-config, git 2.39 or newer, `just`, and the development packages listed in the
[README](README.md#the-binary). For the full check you also need `strace` (the socket audit),
`cargo-deny` (`cargo install --locked cargo-deny`) and Python 3 (two of the audits are scripts).
With `git-lfs` and `ssh-keygen` installed, the LFS and signature tests run instead of skipping
themselves.

```sh
cargo build -p gitweft                   # a debug build
cargo run -p gitweft -- ~/code/project   # run it on a repository
just check                               # everything CI runs (see the end of this file)
```

Debug builds compile the dependencies with optimisations (`[profile.dev.package."*"]` in
`Cargo.toml`), so even a debug build is usable on a large repository.

## Finding your way around

The workspace has eleven crates under `crates/`:

| Crate | What it is |
|---|---|
| `gitweft` | The binary. `main.rs` decides from the arguments whether to open the window, run the headless smoke test, or act as a helper for git. |
| `gitweft-workspace` | The application: the window, tabs, every view, the command palette, key bindings, translations. |
| `gitweft-ui` | The design system: colors (`Palette`), sizes (`Metrics`), fonts, icons, the settings store. |
| `gitweft-core` | Loading a repository, background jobs, file watching, the undo log, review sessions, settings, the trash. No UI code. |
| `gitweft-git` | Talking to git: `read/` reads with gix, `cli/` runs the system `git`, the other modules are the operations. No UI code. |
| `gitweft-graph` | Lays out the commit graph: which lane and color each commit gets, and the lines between them. |
| `gitweft-diff` | Word-level diffs, line pairing, moved lines, conflict markers. |
| `gitweft-syntax` | Syntax highlighting. |
| `gitweft-platform` | What differs between Linux, macOS and Windows: paths from git's bytes, folders, process groups, local sockets, lock holders. The only crate that calls the operating system directly. |
| `gitweft-console` | `gitweft.com` on Windows, a small console program that runs `gitweft.exe` and waits for it. |
| `gitweft-testkit` | Throwaway git repositories for tests. |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) goes through how they fit together, from a click to
the git process and back. It is worth reading once before changing anything bigger than a label.

`site/` is the project page at https://sorrelfox.github.io/gitweft/: one HTML file with no
JavaScript and nothing loaded from elsewhere. `.github/workflows/pages.yml` adds the icon and the
screenshots and publishes it whenever one of them changes on `main`. The `.txt` file named by a
hex number is the page's IndexNow key, which lets search engines be told when the page changes.

## The rules

These are the things a review will ask about. Each one exists for a reason, so the reason comes
with it.

### 1. Nothing waits for git on the UI thread

The thread that draws the window must never run a git command or read a repository. If it did,
the window would freeze for as long as git takes, and on a large repository that is long. So
every piece of git work is a job on a worker thread, and the view awaits its result. Simplified
from `workspace/lfs_ops.rs`:

```rust
let task = self.jobs.spawn(Lane::Latest("lfs-locks"), move |cancel| {
    lfs_locks(&git, &cwd, cancel)
});
cx.spawn_in(window, async move |this, cx| {
    let result = task.join().await;
    this.update_in(cx, |this, window, cx| match result {
        Some(Ok(locks)) => this.open_locks_palette(locks, window, cx),
        Some(Err(err)) => this.report(err, cx),
        None => {} // cancelled, or replaced by a newer job of the same key
    })
})
.detach();
```

The lane says what happens when jobs pile up. `Latest(key)` cancels the running job with the
same key, so a newer selection wins. `Coalesce(key)` keeps at most one job waiting, so a burst
of file changes becomes one status refresh. `Write` runs jobs one at a time on a thread of its
own, so two mutations never overlap. `join()` gives `None` when the job was cancelled or
replaced; that is normal, not an error.

### 2. Every git process goes through `GitCli`

Never `Command::new("git")`; clippy fails the build on it (`clippy.toml`). `gitweft_git::GitCli`
is the one place git is started, and it does a lot for you: a fixed environment
(`GIT_TERMINAL_PROMPT=0` so git never waits for a terminal, `LC_MESSAGES=C` so its messages can
be recognised), a process group of its own so that cancelling stops the whole tree, progress
parsed out of stderr, failures classified into `GitErrorKind` (a conflict, a rejected push, a
missing identity, and so on), the askpass helper wired up so password prompts appear in the app,
and every command recorded in the Activity log. The user's own programs (terminal, editor, file
manager) are the one exception; they start from `gitweft_core::external`.

### 3. No network code, ever

The app process never opens an internet connection; only its git and ssh children do, and only
when the user asks. This is enforced in layers, and a change that breaks any of them is wrong:
`deny.toml` bans HTTP, TLS and telemetry crates; `clippy.toml` bans `std::net` and GPUI's HTTP
client; `scripts/url-audit.sh` fails on any URL literal in the sources; and `just check` runs the
app in a namespace without a network and under `strace` to prove that no internet socket was
opened. [docs/OFFLINE.md](docs/OFFLINE.md) has the whole story.

### 4. Reads with gix, writes with git

Anything that only reads a repository uses gix in-process (`gitweft-git/src/read/`): it is fast
and needs no process. Anything that changes a repository or talks to a remote uses the system
git through `GitCli`, so hooks, signing, credential helpers and LFS behave exactly as in a
terminal. When gix cannot read something correctly, the fallback is git in read-only mode,
`git.command(dir).args([...]).read_only()`, which also tells git never to fetch missing objects on
its own.

### 5. Paths and ref names are bytes

A file name or a branch name is not always valid UTF-8. Keep them as `BString` (from gix) or
`OsString`, hand git the bytes, and convert to text only to show them (lossily, with the
replacement character). Converting early loses files.

### 6. Every piece of text on screen is a key in `locales/app.yml`

The code never contains an English sentence. It reads one:

```rust
t!("branch.invalid_name", name = name)
tn!("history.count", count = revisions.len())   // history.count_one, history.count_other
```

A few rules follow from this. A message is one key with placeholders, never pieces joined in
code, because word order differs between languages. A counted string has `_one` and `_other`
forms (and `_few` and `_many` in languages that need them) and is read with `tn!`. A YAML value
containing `: ` must be quoted. The test in `crates/gitweft-workspace/tests/locale_keys.rs`
checks that every key the code reads exists, that every key is read, and that counted strings
have their forms.

To see where every text on screen comes from, run with `GITWEFT_LOCALE=en-XA`: every translated
text shows «áccéntéd», so plain English is a string that bypassed the file, and a sentence shown
as two accented pieces was glued together in code.

The other twelve languages live in `locales/<lang>.yml`. The scripts in `scripts/i18n/` turn
`app.yml` into a JSON file for a translator, check a translation (every key present, the right
plural forms, the same placeholders) and write it back as a locale file.

### 7. Colors from the palette, sizes from the metrics

Views do not use gpui-kit's theme fields, hard-coded pixel sizes or the kit's text size classes.
They ask `cx.palette()` for a color and `cx.metrics()` for a size, so the themes, the user's own
colors, the text size setting and the zoom apply everywhere at once. `scripts/style-audit.py`
(part of `just check`) fails on the usual ways around this; a line that must keep a fixed size
says why in a trailing `// unscaled: ...` comment.

### 8. Fonts, icons, themes and syntaxes are compiled in

Nothing is loaded from the network at run time, and nothing from the user's disk except a font
they chose from the ones installed on their system. Resolve a font family before applying it;
GPUI panics on a family it cannot find.

### 9. `gpui-kit` is pinned exactly

GPUI has no stable release yet, so `Cargo.toml` pins `gpui-kit = "=0.6.1"`. Upgrade it on
purpose, in a commit of its own, never as a side effect of another change. The kit's sources are
in `~/.cargo/registry/src/*/gpui-component-*` and the crates next to it; read them to check an
API rather than guessing.

## Where the app keeps things

| What | Where |
|---|---|
| Settings | `$XDG_CONFIG_HOME/gitweft/settings.json` (usually `~/.config/gitweft/`) |
| Per-repository state: the undo log, review sessions, repository settings | `$XDG_DATA_HOME/gitweft/repos/<id>/` (usually under `~/.local/share/gitweft/`) |
| The trash | `$XDG_DATA_HOME/gitweft/trash/` |
| The last session and the recent repositories | `$XDG_DATA_HOME/gitweft/` |
| Sockets (single instance, credential prompts) | `$XDG_RUNTIME_DIR/gitweft/` |

Tests never touch these: each gets directories of its own.

## Tests

`cargo test --workspace` runs everything. There are four kinds:

- **Unit tests** next to the code.
- **Integration tests** in each crate's `tests/`, which run the real git in throwaway
  repositories. `gitweft_testkit::Sandbox` makes one, with its own HOME and git config, a fixed
  author and a fixed date, so commits are deterministic and your own `~/.gitconfig` is never
  read or written.
- **View tests** (`crates/gitweft-workspace/src/ui_test.rs` and the views' own test modules),
  which drive real views in a headless GPUI window by keystroke and mouse: the staging lists, the
  graph, the rebase editor, the command palette, the key recorder.
- **Differential tests**, which generate repositories with proptest and compare Gitweft's history
  walk and diff parsing with git's own output.

Some tests skip themselves with a note when a tool is missing (git-lfs, ssh-keygen, a terminal
emulator). To run one test: `cargo test -p gitweft-git --test merge name_of_the_test`.

`just test-git 2.39.5` builds that git version from source into `target/git/` and runs the suite
with it; 2.39 is the oldest version supported, 2.43 is Ubuntu 24.04's. `just bench` runs the
criterion benchmarks for the graph layout, the diff algorithms and highlighting.

## Before you open a pull request

Run `just check`. In this order it runs `cargo fmt --check`, the packaging version check, the URL
audit, the style audit, clippy with warnings as errors (debug and release), cargo-deny, the
tests, the headless smoke test without a network, and the socket audit. CI runs the same command
on every push, so a green `just check` is what a green CI will look like.

A few habits that make reviews easy: one change per commit, with a message that says what
changed and why; a test for a fixed bug, so it stays fixed; and new text on screen added to
`locales/app.yml` in the same commit as the code that shows it. The other languages can follow
later; a missing translation falls back to English.

## Adding a language

1. `python3 scripts/i18n/source.py` writes `target/i18n/en.json`: every key and its English
   text.
2. Translate it into `target/i18n/<lang>.json`, keeping the `%{placeholders}` as they are.
3. Add the language and its plural forms to `scripts/i18n/langs.py`.
4. `python3 scripts/i18n/check.py <lang>` lists what is missing or wrong;
   `python3 scripts/i18n/build.py <lang>` writes `locales/<lang>.yml`.
5. Add the summary, description and screenshot captions to the desktop file and the metainfo in
   `packaging/linux/`, then run `just check`.

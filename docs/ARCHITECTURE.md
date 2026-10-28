# How Gitweft is put together

A tour of the code for someone who has never opened it. It starts with what happens when you
open a repository, then looks at each layer. The crate names are the folders under `crates/`.

## The short version

Gitweft is a Rust workspace. `gitweft-workspace` is the application you see: a window drawn by
GPUI, with tabs, views and dialogs. It never touches git itself. It asks `gitweft-core` to load
and watch a repository, and `gitweft-core` runs that work on background threads, using
`gitweft-git` to read the repository in-process with gix and to run your own `git` for anything
that changes it. The three small crates (`gitweft-graph`, `gitweft-diff`, `gitweft-syntax`) are
pure functions over data: lay out a graph, pair two lines, color a line of code. Nothing below
`gitweft-ui` knows that a user interface exists, which is what makes most of the app testable
without a window.

```
gitweft                  the binary: picks the mode from the arguments
└── gitweft-workspace    the window, tabs, views, actions, translations   (uses GPUI)
    ├── gitweft-ui       palette, metrics, fonts, icons, settings store   (uses GPUI)
    ├── gitweft-core     repositories, jobs, watcher, undo, review, trash
    │   └── gitweft-git  read/ with gix, cli/ with the system git, the operations
    │       └── gitweft-platform  folders, files, processes, local sockets per system
    ├── gitweft-graph    lane layout
    ├── gitweft-diff     word diffs, moved lines, conflict markers
    ├── gitweft-syntax   highlighting
    └── gitweft-testkit  sandboxed repositories, for tests only
```

Dependencies point downwards only: the views depend on the core, the core never depends on the
views.

## What happens when you open a repository

1. You pick a folder on the start screen, or pass it on the command line. The `Workspace` of
   the new tab asks its `RepoJobs` to run `gitweft_core::repo::load_repo` on a worker thread.
2. `load_repo` opens the repository with gix, reads the refs, and walks the history. When the
   repository has a commit-graph file, gix walks it; without one, `git rev-list` is quicker, so
   the walk runs there instead. The commits go through `gitweft_graph::LayoutBuilder`, which
   gives each one a lane and a color and produces the lines between them as data.
3. The result comes back to the UI thread over a channel. The graph view keeps it and draws only
   the rows on screen; the sidebar shows the branches, tags and stashes that were found; a
   status job starts, to list the working changes.
4. A file watcher starts for the repository (`gitweft-core/src/watch.rs`). When files, the index
   or the refs change, it asks for the right refresh, and the job lanes turn a storm of events
   into one refresh instead of a hundred.
5. From then on, every click that needs git becomes a job. The view that asked awaits the result
   and redraws. The window itself never waits.

## The process, and its other jobs

`crates/gitweft/src/main.rs` looks at the arguments before it touches the display, because git
runs the same binary as a helper:

| Invocation | What it does |
|---|---|
| `gitweft [PATH...]` | The window, one tab per path. If a Gitweft is already running, the paths are handed to it over a local socket (`gitweft-core/src/ipc.rs`) and this process exits; `--new-instance` skips the hand-off. |
| `gitweft --headless-smoke [PATH]` | Loads the repository without a window and prints timings. The offline checks run this. |
| `gitweft --sequence-editor PLAN TODO` | Git runs this as `GIT_SEQUENCE_EDITOR` during an interactive rebase: it writes the plan Gitweft prepared into git's todo file. |
| as askpass | Git and ssh run the binary as `GIT_ASKPASS` and `SSH_ASKPASS`, recognised by an environment variable that only git children of Gitweft have. The helper sends the prompt over a local socket to the running window, which shows a dialog, and prints the answer back to git (`gitweft-git/src/askpass.rs`). |
| `--version`, `--help` | What you expect. |

Git finds the helper as `/proc/<pid>/exe`, which is always the running binary, even after a
package update has replaced the file on disk.

## Threads and job lanes

The rule: no git work on the UI thread. `gitweft-core/src/jobs.rs` has one `WorkerPool` for the
whole application (two to eight threads, by the machine) and a `RepoJobs` per open repository.
Every job is spawned into a lane:

| Lane | Behaviour | Typical use |
|---|---|---|
| `Latest(key)` | A new job cancels the running job with the same key. | The diff of the selected file: a newer selection wins. |
| `Coalesce(key)` | One job runs, one waits; a new request replaces the waiting one. | A status refresh after a burst of file changes. |
| `Write` | A single thread, strictly in order. | Commits, merges, pushes: anything that changes the repository, so two never overlap. |

A job gets a `CancelToken` and is expected to poll it (gix and the git runner take it). Its
result arrives through a `JobHandle`, which the view awaits in `cx.spawn`. A cancelled or
replaced job yields `None`. `RepoJobs::pending_writes()` is what "this tab is busy" means when
you try to close it.

## Reading with gix, writing with git

`gitweft-git/src/read/` reads in-process with gix: the history walk, refs, trees, blame, diffs,
the reflog, dangling commits, the content search. Each module is one kind of read. When gix
cannot do something correctly, the fallback is the git command line in read-only mode.

`gitweft-git/src/cli/` is the runner, `GitCli`. Every git process in the application starts here
(clippy forbids `Command::new("git")` anywhere else). It:

- sets a fixed environment: `GIT_TERMINAL_PROMPT=0`, `LC_MESSAGES=C`, the askpass helper, and for
  read-only runs `GIT_NO_LAZY_FETCH=1`, so a partial clone never fetches on its own;
- starts each child as a process tree of its own (a process group on Linux and macOS, a job
  object on Windows), so that cancelling stops the whole tree, the ssh it started included;
- streams stderr and parses git's progress lines, which is where the progress bars come from;
- classifies failures into `GitErrorKind` (a conflict, a rejected push, a missing identity, an
  untrusted directory, and so on), which is how the UI knows what to offer next;
- records every command, with its arguments, exit code, duration and output, in the Activity log.

The modules next to `cli/` (`branch.rs`, `merge.rs`, `rebase.rs`, `remote.rs`, `stash.rs`,
`lfs.rs` and the others) are the operations, one file per area. Ref names and paths stay
`BString` or `OsString` all the way through and become text only for display, because they need
not be UTF-8.

## Undo, and the other state on disk

All of this is local; none of it leaves the machine.

- **Undo and redo** (`gitweft-core/src/oplog.rs`). Each repository has an `oplog.jsonl` in its
  data folder: one JSON object per line, the undo stack and then the redo stack. An entry records
  which refs an operation moved, from what to what. Undo is a compare-and-swap ref update: a ref
  moves back only if it is still where the operation left it, so undo can never discard newer
  work. A hard reset also saves the untracked files it would remove, and a deleted branch's
  config, so undo restores those too. The log is shared between tabs and processes through a
  file lock. Operations the log does not model (fetch, push, switching branches, stashes) are not
  undoable; switching back is their undo.
- **Review sessions** (`review.rs`): what a branch is reviewed against, which files were marked
  viewed at which content, and the comments.
- **The trash** (`trash.rs`): what Gitweft moved aside instead of deleting (discarded changes,
  cleaned files), so it can be restored.
- **The session** (`session.rs`): the open tabs, the active one and the window size, restored at
  the next start.
- **Per-repository settings** (`repo_settings.rs`): walk options, hidden branches, background
  fetch.

The data for a repository lives under `$XDG_DATA_HOME/gitweft/repos/<id>/`. The id is a
`gitweft.id` value written once into the repository's own `.git/config`, so the data follows the
repository when it is moved or renamed. App-wide settings are in
`$XDG_CONFIG_HOME/gitweft/settings.json` (`settings.rs`): a versioned JSON file that tolerates
unknown keys and is never overwritten by an older build. Those are the Linux folders; on macOS
both are under `~/Library/Application Support`, on Windows under `%LOCALAPPDATA%` and
`%APPDATA%`, unless the XDG variables are set.

## The UI layer

`gitweft-workspace` is organised like this:

- `shell.rs`: the window, the title bar, the tab strip. Each tab owns a `Workspace`.
- `workspace.rs` and `workspace/`: one repository's toolbar, sidebar, centre and right panel,
  and the operations (`branch_ops.rs`, `remote_ops.rs`, `rewrite_ops.rs`, `undo_ops.rs` and the
  rest), each spawning its jobs.
- The views: `graph_view.rs`, `staging_view.rs`, `diff_view.rs`, `details_view.rs`,
  `conflict_view.rs`, `rebase_view.rs`, `blame_view.rs`, `compare_view.rs`, `trash_view.rs` and
  so on. The centre is a stack: opening a view from a lower one puts it on top, closing it goes
  back down.
- `palette.rs`: the command palette, which also finds branches, tags, files and recent
  repositories, and shares its data with the row menus so the two never drift apart.
- `keymap.rs`: the key bindings, the user's changes to them, and the recorder in the settings.
- `settings_window.rs` and `settings_ui.rs`: the settings window, a second window with a live
  preview in the main one.
- `locale.rs` and `plural.rs`: which language, and the plural forms.

Views take colors from `cx.palette()` and sizes from `cx.metrics()` (`gitweft-ui`), never from
gpui-kit's theme fields or pixel literals, and the kit's own theme is built from the same
palette, so kit widgets and Gitweft's views agree. Every user-visible string is a key in
`locales/app.yml`, read with `t!` or `tn!`.

## Linux, macOS and Windows

Everything that differs between the systems lives in `gitweft-platform`: turning the bytes git
prints into paths, where the data and settings folders are, private folders and the executable
bit, starting a process tree and stopping it, and the local sockets (Unix sockets; on Windows,
the AF_UNIX sockets Windows 10 and 11 have). It is the only crate that uses `std::os`, `libc` or
`windows-sys`, so the crates above it are the same code on every system. cargo-deny checks the
dependency graph of all three.

## In the Flatpak

The Flatpak brings its own git, because the Freedesktop runtime has none. The user's credential
helpers, hooks, terminal, editor and diff tools are host programs, so `gitweft-git/src/flatpak.rs`
and `gitweft-core/src/external.rs` run them on the host through `flatpak-spawn --host` when that
is available, and inside the sandbox otherwise.

## Tests

- Unit and integration tests per crate; `gitweft_testkit::Sandbox` gives a test a repository of
  its own, with an isolated HOME, a fixed identity and a fixed date.
- View tests drive real views in a headless GPUI window (`TestAppContext`): keystrokes, clicks and
  drags, then assertions against git's state and the view's.
- Differential tests (proptest) generate repositories and compare the history walk and the diff
  parsing with git's own output.
- `just check` ties it together with formatting, clippy, cargo-deny, the audits and the offline
  smoke test. [CONTRIBUTING.md](../CONTRIBUTING.md) has the details.

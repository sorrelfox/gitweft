# Changelog

What changed in each release of Gitweft, newest first.

The first paragraph of an entry is the release note that travels with the packages. The
same sentence sits in the AppStream metainfo
(`packaging/linux/io.github.sorrelfox.Gitweft.metainfo.xml`), which the RPM and DEB
changelogs are written from, and `just versions` fails when the two disagree. So when you
write an entry here, write the same first paragraph there.

## 1.0.2 (2026-10-28)

Gitweft now runs on macOS and Windows too: a disk image with one app for Apple silicon and
Intel Macs, and an installer and a zip for 64-bit Windows. Both are new and less tested than the
Linux packages, and neither is signed yet, so macOS and Windows ask once before opening them.

In this release:

- macOS: ⌘ in place of Ctrl for the shortcuts, the menu bar, folders opened from Finder or
  with `open -a Gitweft`, Terminal and Finder as the external tools, and the oldest macOS it
  runs on is 10.15.7.
- Windows: an installer that needs no administrator rights, Windows Terminal or cmd and
  Explorer as the external tools, and `gitweft.com` next to `gitweft.exe`, so that
  `gitweft --version` in cmd or PowerShell prints before the prompt comes back.
- When git reports a lock file, Gitweft can now tell on macOS and Windows too whether a git
  that is still running holds it.

## 1.0.1 (2026-10-28)

Error messages name the files git lists, such as the local changes a checkout or a stash would
overwrite, every release package comes with a build attestation that the GitHub CLI can verify,
and the RPM, DEB and AppImage are now built for 64-bit ARM (aarch64) too.

## 1.0.0 (2026-10-27)

The first release: a commit graph that stays quick on very large repositories, staging by
file, hunk or line, a merge conflict tool, an interactive rebase editor, local code review,
undo for local operations, twelve interface languages, and RPM, DEB, AppImage and Flatpak
packages.

In this release:

- The "Graphite" look, in dark, light and high-contrast themes, with your own color sets
  saved under a name, and a settings window for fonts, text sizes, density, dates, external
  tools and keyboard shortcuts.
- The graph: colored lanes, branch and tag pills, signature marks, search by text or by
  content, walk options per repository (current branch only, first parent, topological
  order), hidden and soloed branches, several commits selected at once.
- Interactive rebase with a plan editor (reorder by drag, squash messages edited in place,
  pauses, autosquash, a preview of the result) and quick reword, squash, fixup and drop
  from the graph.
- Local code review: compare any two branches, tags or commits, mark files viewed, comment
  on lines, see what changed since your last look, and export the review as Markdown or a
  git note.
- Blame with "blame the parent" and ignored commits, file history that follows renames, a
  file browser at any commit, the reflog with an "only lost commits" filter, dangling
  commits, and bisect by hand or by command.
- Worktrees, submodules, Git LFS (including file locks), shallow and partial clones, an
  optional background fetch, and credential prompts shown in the app.
- A trash for everything discarded, undo and redo that work across tabs and processes,
  and repairs for a stale index lock and folders git does not trust.
- Translations into German, French, Spanish, Portuguese (Brazil), Italian, Polish,
  Russian, Ukrainian, Chinese (Simplified), Japanese, Korean and Turkish.
- Reproducible packages built from vendored sources, each installed and smoke-tested
  without a network before it is released.

## 0.1.0 (2026-09-12)

Development build: graph, staging, commits, branches, remotes, merge conflicts, tabs and undo.

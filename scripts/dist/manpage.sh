#!/usr/bin/env bash
# gitweft(1), made from the binary's own `--help` (so the options can't drift from the CLI)
# and `--version`, with the files and environment it uses. English: the help is read with
# GITWEFT_LOCALE=en. Dated by the metainfo's newest <release> (release-entry.sh), so the
# page is the same on every build of a release.
# Usage: scripts/dist/manpage.sh BINARY OUT   (e.g. target/dist/gitweft $root/usr/share/man/man1/gitweft.1)
# An OUT ending in .gz is compressed (gzip -9n, as Debian wants).
set -euo pipefail
binary=$(realpath "${1:?usage: manpage.sh BINARY OUT}")
out=${2:?usage: manpage.sh BINARY OUT}
root=$(cd "$(dirname "$0")/../.." && pwd)

help=$(GITWEFT_LOCALE=en "$binary" --help)
version=$("$binary" --version | awk '{print $2}')
date=$(bash "$root/scripts/dist/release-entry.sh" "$root" | head -1)

# roff: a backslash doubled, a hyphen made a minus (options are typed), a line that starts
# with a dot or an apostrophe kept from being read as a request.
esc() { sed -e 's/\\/\\e/g' -e 's/-/\\-/g' -e "s/^\\([.']\\)/\\\\\\&\\1/"; }

usage=$(sed -n '1s/^Usage: //p' <<< "$help")
# The paragraphs between the usage line and "Options:".
description=$(sed -n '2,/^Options:/p' <<< "$help" | sed '/^Options:/d' | sed '/./,$!d')
options=$(sed -n '/^Options:/,$p' <<< "$help" | tail -n +2)

page=$(
    printf '.TH GITWEFT 1 "%s" "gitweft %s" "User Commands"\n' "$date" "$version"
    printf '.SH NAME\ngitweft \\- a fast, offline Git client\n'
    printf '.SH SYNOPSIS\n.B gitweft\n%s\n' "$(sed 's/^gitweft //' <<< "$usage" | esc)"
    printf '.SH DESCRIPTION\n'
    # A blank line starts a new paragraph.
    esc <<< "$description" | sed 's/^$/.PP/'
    printf '.SH OPTIONS\n'
    # "  --flag [ARG]  What it does": the flag part, then the text after two or more spaces.
    while IFS= read -r line; do
        [ -z "${line// /}" ] && continue
        flag=$(sed -E 's/^ +//; s/ {2,}.*$//' <<< "$line")
        text=$(sed -E 's/^ +([^ ]| [^ ])* {2,}//' <<< "$line")
        printf '.TP\n.B %s\n%s\n' "$(esc <<< "$flag")" "$(esc <<< "$text")"
    done <<< "$options"
    cat << 'EOF'
.SH FILES
.TP
.I ~/.config/gitweft/settings.json
Settings (\fB$XDG_CONFIG_HOME/gitweft\fP when set).
.TP
.I ~/.local/share/gitweft/
Per\-repository state (undo history, reviews, settings), the trash of discarded files and
the last session (\fB$XDG_DATA_HOME/gitweft\fP when set). Nothing is sent anywhere: the
app has no network code of its own; git fetches and pushes only when asked.
.SH ENVIRONMENT
.TP
.B GITWEFT_LOCALE
The language of the interface, instead of the one \fBLANGUAGE\fP, \fBLC_ALL\fP,
\fBLC_MESSAGES\fP or \fBLANG\fP choose.
.SH SEE ALSO
.BR git (1)
EOF
)

mkdir -p "$(dirname "$out")"
case $out in
    *.gz) printf '%s\n' "$page" | gzip -9n > "$out" ;;
    *) printf '%s\n' "$page" > "$out" ;;
esac

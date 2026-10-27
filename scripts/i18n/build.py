#!/usr/bin/env python3
"""Writes locales/LANG.yml from a checked translation (#46).

Usage: scripts/i18n/build.py LANG [JSON]   (default target/i18n/LANG.json)
The file is rust-i18n's one-language format (`_version: 1`, the locale is the file name):
one `key: "text"` line per key, app.yml's plain keys in its order and then the counted
strings' forms, each text a JSON string (which YAML reads as a double-quoted scalar); the
locale-keys test reads it the same way.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from check import expected, load, problems  # noqa: E402
from langs import LANGUAGES  # noqa: E402
from source import ROOT  # noqa: E402


def main():
    lang = sys.argv[1]
    path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(ROOT, "target", "i18n", f"{lang}.json")
    translation = load(path)
    found = problems(lang, translation)
    if found:
        sys.exit(f"{len(found)} problems (scripts/i18n/check.py {lang} lists them)")
    name = LANGUAGES[lang][0]
    lines = [
        "_version: 1",
        f"# {name}: locales/app.yml translated. Every key of app.yml, with this language's",
        "# plural forms; checked by crates/gitweft-workspace/tests/locale_keys.rs.",
        "",
    ]
    for key in expected(lang):
        lines.append(f"{key}: {json.dumps(translation[key], ensure_ascii=False)}")
    out = os.path.join(ROOT, "locales", f"{lang}.yml")
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()

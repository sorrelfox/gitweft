#!/usr/bin/env python3
"""Checks a translation against the English source (#46).

Usage: scripts/i18n/check.py LANG [FILE]
FILE is the translation as JSON ({key: text}, default target/i18n/LANG.json) or as the
built locales/LANG.yml. It must have every key of locales/app.yml, and for a counted
string exactly the plural forms LANG uses (scripts/i18n/langs.py); each text keeps the
English text's %{placeholders} (a plural form may leave out %{count}, as English's "one"
forms sometimes do). Prints the problems and exits 1 if there are any.
"""
import json
import os
import re
import sys

import yaml

sys.path.insert(0, os.path.dirname(__file__))
from langs import LANGUAGES  # noqa: E402
from source import FORMS, ROOT, english, plural_bases  # noqa: E402

PLACEHOLDER = re.compile(r"%\{([a-z_]+)\}")


def placeholders(text):
    return set(PLACEHOLDER.findall(text))


def expected(lang):
    keys = english()
    english_text = dict(keys)
    bases = plural_bases(keys)
    forms = LANGUAGES[lang][1]
    wanted = {}
    for key, text in keys:
        if any(key.endswith(form) for form in FORMS):
            continue
        wanted[key] = placeholders(text)
    for base in bases:
        allowed = set()
        for form in FORMS:
            allowed |= placeholders(english_text.get(base + form, ""))
        for form in forms:
            wanted[f"{base}_{form}"] = allowed
    return wanted


def load(path):
    with open(path, encoding="utf-8") as f:
        if path.endswith(".json"):
            return json.load(f)
        data = yaml.safe_load(f)
        return {k: v for k, v in data.items() if not k.startswith("_")}


def problems(lang, translation):
    wanted = expected(lang)
    found = []
    for key in wanted:
        if key not in translation:
            found.append(f"missing: {key}")
    for key, text in translation.items():
        if key not in wanted:
            found.append(f"not a key for {lang}: {key}")
            continue
        # A separator may be a space (French, Polish, Russian use a narrow no-break space
        # between thousands); any other text needs more than whitespace.
        blank = not text.strip() if isinstance(text, str) and key != "number.thousands_separator" else not text
        if not isinstance(text, str) or blank:
            found.append(f"empty or not text: {key}")
            continue
        have = placeholders(text)
        allowed = wanted[key]
        is_form = any(key.endswith("_" + form) for form in ("one", "few", "many", "other"))
        must = allowed - {"count"} if is_form else allowed
        if not must <= have or not have <= allowed:
            found.append(f"placeholders of {key}: {sorted(have)}, English has {sorted(allowed)}")
    return found


def main():
    lang = sys.argv[1]
    if lang not in LANGUAGES:
        sys.exit(f"unknown language {lang}; see scripts/i18n/langs.py")
    path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(ROOT, "target", "i18n", f"{lang}.json")
    found = problems(lang, load(path))
    for line in found[:200]:
        print(line)
    if found:
        print(f"{len(found)} problems in {path}")
        sys.exit(1)
    print(f"{path}: {len(expected(lang))} keys, all there")


if __name__ == "__main__":
    main()

"""The languages Gitweft ships besides English, and the plural forms each needs.

The forms are CLDR's categories for whole numbers, as `plural::category` in
crates/gitweft-workspace/src/plural.rs picks them; the locale-keys test holds each file to
this table too.
"""

LANGUAGES = {
    "de": ("German", ["one", "other"]),
    "fr": ("French", ["one", "other"]),
    "es": ("Spanish", ["one", "other"]),
    "pt-BR": ("Portuguese (Brazil)", ["one", "other"]),
    "it": ("Italian", ["one", "other"]),
    "pl": ("Polish", ["one", "few", "many"]),
    "ru": ("Russian", ["one", "few", "many"]),
    "uk": ("Ukrainian", ["one", "few", "many"]),
    "zh-CN": ("Chinese (Simplified)", ["other"]),
    "ja": ("Japanese", ["other"]),
    "ko": ("Korean", ["other"]),
    "tr": ("Turkish", ["one", "other"]),
}

FORMS = ("one", "few", "many", "other")

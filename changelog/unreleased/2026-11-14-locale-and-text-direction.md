- **The runtime now tells a page — and a screen reader — what language its text is in, and
which way it lays out.** This is the code-portable half of C4 (i18n / RTL / bidi). Three pieces:
an application can declare its interface language in its manifest
(`default_locale`, a BCP 47 tag, validated — `"he"` or `"ar"` means the interface lays out
right-to-left); a GPU island's accessible description now carries `locale` and `direction` next
to its label and rows, so a screen reader is told the language and reads a right-to-left island
the way its script is written; and `describe` — the call a page makes to learn where it is —
answers with the platform's `locale` and `text_direction`, derived from the locale rather than
asserted separately, so a page never keeps its own table of which scripts run right-to-left.

**Why the direction is derived, not stored.** `TextDirection::for_locale` maps a BCP 47 tag's
language subtag to a direction against a fixed set of right-to-left scripts (Arabic, Hebrew,
Farsi, Urdu, and the others), case-insensitively and ignoring script/region subtags, so `he`,
`he-IL`, and `HE` all name Hebrew. An unrecognised tag defaults to left-to-right — the
right-to-left set is small and fixed, so an unknown or typo'd locale degrades to the common
direction rather than flipping a page. This is the *metadata* a reader is told, not the shaping
or bidi layout itself: the renderer still draws the glyphs, exactly as before.

**The platform locale is read from the environment, and says so when it cannot know.**
`platform_locale` reads `LC_ALL` then `LANG` and strips a codeset suffix (`en_US.UTF-8` becomes
`en_US`). A platform that exposes its locale another way — Windows reports it through
`GetUserDefaultLocaleName`, an FFI call this runtime does not make — yields `"und"`
(undetermined), so a page is told the locale is unknown rather than handed a wrong one. The FFI
reader is the fix and is a scoped, audited addition under ADR-012 when it comes. The locale and
direction ride to the page on the window seam, so every window's page is told the same matched
pair.

**Measured: deriving the direction adds nanoseconds, not a regression.** The direction is
derived on every `describe` and every island's accessible description, so it is on a hot path.
`locale/direction_for_locale` in `ncui-bench` measures it: **~33 ns** for a right-to-left tag and
~42 ns for a left-to-right one — a language-subtag match against a fixed set, negligible against
the describe/island work around it. The GPU text-shaping half of C4 (RTL/bidi runs, ligatures,
the clipped-row gap) is the part that owns real layout performance, and it is separate.

**Tested end to end.** `TextDirection::for_locale` is unit-tested across right-to-left and
left-to-right tags including case and subtag variations; `is_locale_tag` and `default_locale`
are tested in the manifest (valid tags parse and validate, `"not a locale"`, `"en.US"`, `"../etc"`
are refused); `describe` is tested to report a locale and a direction that agree with each other
and carry no codeset suffix; and a `page_bridge` end-to-end test drives the real page pump — a
page asks `describe` over the bridge and the answer names the locale and a text direction. The
counter example declares `default_locale: "en"`. Gate: `cargo fmt`, `cargo clippy --workspace
--all-targets` (pedantic, no warnings), and the full `cargo test --workspace` suite (1465 tests)
all pass.

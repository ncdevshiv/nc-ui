# ncui-kit-providers

Bridges the `nc-ui-kit` GPUI widget library into NC-UI's native-view host. The crate
registers host-linked providers with `ncui-gpui`; embedding applications opt in at their
composition root by calling `register_all()` before the runtime serves.

## Providers and props

All props are JSON objects. `null` is accepted as the empty object for providers whose
fields all have defaults. Unknown fields are refused, because a mistyped field should
surface where the mistake was made rather than silently changing a control's meaning.
Strings and arrays have the bounds listed below in addition to the runtime's outer
64 KiB props bound.

| Provider id | Props | Events | Commands |
| --- | --- | --- | --- |
| `nc-ui-kit::button` | `label` string 1–128 bytes (default `"Continue"`); `variant` one of `primary`, `secondary`, `outline`, `ghost`, `danger`, `link` (default `secondary`); `size` one of `sm`, `md`, `lg` (default `md`); `disabled`, `loading`, `full_width` booleans (default `false`) | source `button`, action `clicked`, payload `{label, origin}` | `click` with null or `{}` performs the action when neither disabled nor loading |
| `nc-ui-kit::input` | `value` string up to 4096 bytes (default empty); `placeholder` string 1–256 bytes (default `"Type here…"`); `numeric`, `disabled`, `invalid` booleans (default `false`) | source `input`, action `changed`, payload `{value, origin}` | `clear`, `focus`, each with null or `{}`; `focus` is refused while disabled |
| `nc-ui-kit::dropdown` | `options` required array of 1–32 non-empty strings, each up to 128 bytes; `selected` zero-based integer default `0`; `placeholder` string 1–128 bytes (default `"Select…"`) | source `dropdown`, action `selected`, payload `{index, option, origin}` | `open`, `close`, `focus` with null or `{}`; `select` with `{index}` |
| `nc-ui-kit::checkbox` | `checked` boolean (default `false`); `label` string 1–256 bytes (default `"Enabled"`); `disabled` boolean (default `false`) | source `checkbox`, action `toggled`, payload `{checked, origin}` | `toggle` with null or `{}`; `set_checked` with `{checked}`; `focus` with null or `{}` |
| `nc-ui-kit::dialog` | `title` string 1–128 bytes (default `"Confirmation"`); `description` optional string up to 2048 bytes; `trigger_label` string 1–128 bytes (default `"Open dialog"`); `open` boolean (default `true`); `width` number 240–560 (default `420`) | source `dialog`, actions `opened` and `closed`, payload `{title, origin}` | `open` and `close`, each with null or `{}` |
| `nc-ui-kit::settings-tabs` | `active` one of `general`, `appearance`, `advanced` (default `general`); `panels` optional object whose `general`, `appearance` and `advanced` strings are each 1–2048 bytes | source `settings-tabs`, action `selected`, payload `{tab, origin}` | `select` with `{tab}` |

`origin` is `interaction` when the user produced the event inside the widget and
`command` when an accepted provider command produced the state transition. Replacing
props rebuilds the widget child inside the same root view; that is deliberate, because
kit widgets do not expose a uniform partial-update contract.

## Theme and DPI

`apply_theme` maps NC-UI's binary light/dark platform theme onto the kit's global
`Theme`, preferring `nc-light.json` or `nc-dark.json` from the resolved asset layout and
falling back to the kit's built-in palettes when a file is absent or invalid. The kit
exposes no independent scale factor: its dimensions are GPUI logical pixels, and Windows
DPI behaviour comes from GPUI and the executable manifest. There is therefore no honest
provider-level DPI handoff to implement.

## Assets

`NC_UI_KIT_ROOT` is an explicit override for a directory containing `assets/icons` and
`themes`. Without it, resolution looks beside the running executable for that installed
layout, then falls back to the sibling-source-tree location used during development.
`KitAssets` is the filesystem `gpui::AssetSource` for the resolved `assets` directory.

One integration limit is important: `ncui-gpui` currently constructs
`gpui::Application::new()` internally without calling `.with_assets(...)`, and gpui
0.2.2 exposes no post-construction asset-source setter. This crate can resolve and
provide the correct asset source, but cannot install it into the existing engine without
a composition change in `ncui-gpui`. Until that change exists, SVG glyphs inside kit
widgets can be absent in hosted windows; hit regions, text, keyboard interaction and
events do not depend on those glyph files.

## gpui feature unification

This crate pins gpui exactly as `ncui-gpui` does (`0.2.2`, default features off,
`windows-manifest` on). `nc-ui-kit` itself depends on `gpui = "0.2.2"` with default
features. Cargo feature unification means the default features are enabled for the one
gpui build in a graph containing both crates. That is inherited from the kit and is
accepted deliberately; it is not a conflicting second version requirement.

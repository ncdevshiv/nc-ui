- **Native file, save and folder dialogs are a capability, behind the `dialogs` feature.** A
page that holds `Capability::Dialog` gets `dialog.pick_file` and `dialog.save_file`, which put
the platform's own picker in front of the user and return the path *they* chose — the
capability Electron's `dialog` module and Tauri's `dialog` plugin both have. `rfd` is ADR-012's
eighth exception: the picker is an `IFileOpenDialog`, an `NSOpenPanel` or a GTK file chooser,
all foreign functions that `unsafe_code = "forbid"` rejects calling, and `rfd` performs that
FFI internally so this project's own code stays free of `unsafe`. It is confined to
`ncui-native` and behind the `dialogs` feature, off by default.

Not breaking: the capability is grant-gated like every other — a page that is not granted
`Capability::Dialog` is offered no dialog command at all, and the default build is unchanged.

The design follows the proven global-hotkey/tray-icon shape: the `DialogBackend` seam, the
request vocabulary (`open.file`, `open.files`, `save.file`, `pick.folder`), the option and
filter rules, and the validation of the path that comes back are plain Rust in
`ncui-native::dialog` and are tested headlessly in both feature states. The path is treated as
untrusted input — bounded in length, refused for control characters, refused if it is not valid
UTF-8 — because it came from outside the process and is about to be handed to a page. A dialog
does not grant a read: learning a path is separate from `filesystem.read`, and reading the file
at that path is still contained to the application's own directory.

On a build without the feature the command still registers and still enforces the request
rules, then reports `dialog_unavailable` by name rather than claiming success — the same honest
shape the notification command reports on a host with no notifier.

**What is not automated, recorded rather than claimed:** the single real modal open — a person
clicking the picker and choosing a file — is the one step a test cannot drive, because a test
has no person. Everything around the modal is tested; the modal itself needs a one-time human
verification the feature registry does not model. This is recorded in `docs/known-gaps.md`. A
colour dialog is still absent: `rfd` does not provide one.

Unit-tested (request vocabulary, option and filter refusals, path validation, the
single-vs-many outcome rule, the cancel-is-not-a-failure distinction, registration under the
grant and refusal without it, the `dialog_unavailable` path) across `ncui-native` and
`ncui-runtime` in both the default and `dialogs` feature states; `ncui-core`'s
capability-string-mapping invariant test now covers `dialog` too.

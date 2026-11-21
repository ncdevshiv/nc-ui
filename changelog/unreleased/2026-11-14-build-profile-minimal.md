- **`ncui build --profile minimal` ships a window host without the GPU-island renderer, for
4.9 MiB.** Every window host used to carry the island renderer whether the application drew islands
or not, because `wry-host` implied `gpu-islands` — so the 6.4 MiB graphics stack (`wgpu` and the
text shaper) rode along in every bundle. A window-and-a-page application that never renders an
island paid for a subsystem it could not reach. The renderer is now **optional at link time**: the
window host no longer implies it, and `ncui build --profile {standard,minimal}` compiles the profile
it names and bundles that binary.

`standard` (the default) is the full build — window host plus renderer — and measures 11.292 MiB on
the counter example, the size it always was. `minimal` is the window host alone and measures
**4.902 MiB** (binary 4.872 MiB), under the ≤ 5 MiB target A3 commits to and matching the 4.88 MiB
renderer-free build the completion audit measured by hand. The minimal bundle was launched and
opened a real window. A page that asks for an island on `minimal` gets the asset server's 404 — the
island route is `#[cfg(feature = "gpu-islands")]`-gated — which the counter example already renders
as "No island on this machine" rather than failing.

Not a removal of capability: the standard build is byte-for-byte the size it was, and the default
profile is `standard`, so a plain `ncui build` ships exactly what it shipped before. What changed is
that the heavy subsystem is a **choice** rather than a tax. The profile compiles its own feature set
rather than bundling whatever binary is lying in `target/release` — a `--profile minimal` that
shipped a stale full build would be a lie about size, which is the whole point of the flag — and it
reports which profile it built in the JSON payload and the text output (`profile  minimal`), so a
reader can see why a bundle is the size it is.

The size gap to Tauri's 3.2 MiB remains: a window host without the renderer cannot go below ~4.9 MiB
on this toolchain, because that is the WebView2 host itself. That residual is recorded in
`docs/goal-prompt.md` rather than overclaimed as "approaching 3.2 MiB."

Unit-tested: the profile parser accepts the two names it advertises and an unknown name reports both
accepted values, the default profile is `standard` (so no existing build silently shrinks), and the
two profiles compile different feature sets and differ exactly in the renderer. The scoping test now
drives `--profile` with a real profile name. Verified end to end by building both profiles through
the CLI and measuring the bundles: standard 11.292 MiB, minimal 4.902 MiB.

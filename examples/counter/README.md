# The counter example

```sh
ncui run examples/counter
```

A page served over loopback HTTP, calling into the native runtime. The page reads its own
manifest's `notes.txt` through `fs.read_text`, the **Load the WebAssembly plugin** button
loads the component below and asks it to read the same file, the **Open the notes window**
button opens a second window under a name the runtime remembers, and the **Go fullscreen**
button asks the platform to change how the window it is in is presented.

## The window that changes shape

`window_state` takes one of `fullscreen`, `maximized`, `minimized`, `visible` and `focus` and answers with
the state the platform reported *afterwards*. The button's label follows that answer, not the
page's belief: press Escape while fullscreen and the next press asks for the other direction. One
change per call, because two would be asking for whichever the platform applied last, and the
target is always the caller's own window — a page cannot reach a window it does not own.

`window_shape` is the same idea for the rest of a window: the increment button renames the window
to the count it holds, naming only the `title`. A field the page does not name is left alone, so
the window stays where the user put it while its title follows the document.

## The rectangle the GPU drew

The canvas near the bottom of the page is a GPU island: a region of the document whose pixels the
runtime produced on the GPU. The page fetches them from `/__ncui/island` — an ordinary same-origin
GET, because the runtime serves the frame from the asset server that is already the page's origin —
and draws them into a canvas. The browser then composites the island with the rest of the document,
so z-order, clipping and scrolling are the DOM's business rather than the runtime's.

Two things about it are worth knowing. The pixels cross from the GPU to the CPU and then to the
page, so a frame costs about a megabyte at 640×480: this is the composition boundary, not a
zero-copy fast path. And what an island draws is its rows: the rectangles are drawn by the GPU,
and each row's text is shaped with the machine's own font, broken into lines that fit the row,
rasterised into coverage, and composited over those pixels on the CPU. `docs/known-gaps.md`
records both.

## The window that comes back

The notes window is opened with `remember: "notes"`. That name is the whole mechanism: the runtime
writes down where the window was when it closes, in `windows.json` beside the application's own
window state, and applies that place while the *next* run is creating the window. Move the notes
window, quit the application, run it again and press the button: it comes back where you left it.

The name belongs to the page rather than to the runtime, because the page is what knows that two
windows it opens are two windows. `notes.html` finds itself in `list_windows` and prints the place
it is at, so the remembered geometry is visible rather than taken on faith.

## What the plugin demonstrates

`plugins/greeter.wat` is a WebAssembly component: code this page did not write, running inside
the runtime's process. It is the whole component story in one small artifact.

* **It is text, not a blob.** The engine assembles WAT, so what ships here is reviewable source
  whose provenance a reader can check. `tools/make_greeter_component.py` generates it, and
  `make policy` fails if the file in the tree differs from the generator's output — the offsets
  and lengths inside it are derived from the strings they belong to rather than typed by hand.
* **It declares what it wants.** Its manifest says it imports one service, `fs.read_text`, which
  needs `filesystem.read`, and that it exports `echo` and `greet`. The runtime enforces both
  lists: a method that is not on the export list cannot be called, and a service that is not on
  the import list cannot be served.
* **It cannot read the file itself.** `greet` asks the host, and the host serves the call only
  because the negotiation granted the capability behind the declared service. The component's
  grant is the intersection of what it asked for and what the page holds — it does not inherit
  the page's authority, and it does not get authority the page lacks.
* **It is bounded.** Fuel stops a call that does not return, the store's memory limiter stops
  growth past a cap the component was never offered, and every read out of its memory is bounded
  by a copy budget.

## Why the button asks for consent

`run.components` is a destructive capability: it is permission to execute code nobody in this
project wrote. Every call that loads or drives a component therefore states `consent: true`, and
a call that does not state it is treated as withholding it. Remove `run.components` from
`ncui.app.json` and the `component.*` commands do not exist at all — the page gets a refusal
naming an unknown action rather than a surface that refuses.

## Shipping it

```sh
make bundle
# or, with the runtime already built:
ncui build examples/counter
```

`ncui build examples/counter` writes a portable directory under `dist/`: the runtime binary, this
application, a `bundle.json` describing every file with its size and digest, and — on Windows —
`register-identity.ps1`, which registers the application's identity (`com.ncui.counter`, named in
the manifest) with the platform so its notifications arrive under this application's name rather
than the shell's. The build writes nothing outside that directory: registering an identity is an
install step, and a build tool that quietly rewrote the registry would be a build tool whose
output depended on the machine that ran it.

`ncui build examples/counter --check` verifies the bundle in `dist` against both its own manifest
and a fresh build of this tree, which is how a bundle that no longer matches the application is
caught rather than shipped. It writes nothing: it builds into a private directory and compares.

## A link that reaches the running application

The manifest names a scheme, `ncui-counter`, and the bundle ships what registers it
(`register-protocol.ps1` on Windows, `install-protocol.sh` on Linux). Running that script once
tells the platform that a `ncui-counter://...` URL belongs to this application, and the
registration runs `ncui run <this directory> "%1"`.

The two halves fit together: a link launches the application with the URL as an argument, and the
single-instance hand-off delivers that argument to the instance that is already running instead of
opening a second window. Handing over costs about half a millisecond against about forty-six to
start a process, which is why the link reaches the window the user has.

## Installing it

```sh
ncui install dist/counter
ncui installed
ncui launch counter
ncui uninstall counter
```

`ncui install` copies the bundle into this user's own applications directory, registers the
identity (`com.ncui.counter`) and the URL scheme (`ncui-counter`) the bundle names, and writes
`installed.json` recording what it did. The registrations name the *installed* program's path, so
they come after the files are in place; the record is what makes `ncui uninstall` remove exactly
what was installed rather than whatever the bundle happens to say later.

Nothing is written outside that directory, and an install over an existing one is refused rather
than merged — a half-replaced application is worse than one that is clearly still the old version.

`ncui launch counter` resolves the name and then runs the application exactly as `ncui run` does:
the same single-instance claim, the same remembered window, and the same handling of a link the
platform hands over. A second way of starting an application would be a second set of behaviours
to keep in step.

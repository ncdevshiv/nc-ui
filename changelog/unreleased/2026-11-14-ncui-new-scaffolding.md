- **`ncui new <directory>` scaffolds a runnable application in one command.** First-run-to-window
was a blank directory and a page of documentation; it is now one command that writes the smallest
application that opens a window and calls into the runtime — a manifest (`ncui.app.json`), an
entry page (`public/index.html`) and a script (`public/app.js`). A developer starts from something
that runs and edits it, rather than from nothing.

The scaffold teaches the right habits on day one. It grants the **smallest** capability set that
opens a window and answers the page's first calls (`read.metrics` and `window`) — not everything,
because a template that granted the whole surface on a first run is how an application ends up
holding capabilities it never uses. Its entry page includes the reload bridge and its script
designates `window.__ncui_state`, so the dev loop the runtime ships (state-preserving `--watch`
reload) is the one the template demonstrates. The page's first call is `describe`, so a new
application proves the bridge is live before it does anything else.

Not breaking, and safe by default: `ncui new` **refuses** to scaffold over an existing non-empty
directory, because scaffolding over work in progress would destroy it, and the refusal leaves the
existing files untouched. A name that is empty, a relative hop (`.` / `..`), or carries a character
that cannot appear in a directory name is refused with a reason; a trailing slash and a nested path
are both accepted, and the application is named after the directory's final segment (`ncui new
path/to/my-app` names it `my-app`). The scaffolded manifest is verified by the real `AppManifest`
loader in tests, so `ncui run` on a scaffolded directory gets past the manifest rather than failing
on the first thing it reads.

`ncui-cli` gained the `New` command (parsed, dispatched before the runtime starts like `run`, since
scaffolding touches only the filesystem), a text renderer that prints what was written and the one
command that opens it, and the `new_app` implementation with its templates. Unit-tested: the three
files are written, the scaffolded manifest loads, the page includes the reload bridge and the script
designates state, an existing application is refused with its files untouched, and hostile directory
names are refused while trailing slashes and nested paths are accepted. Verified end to end by
running the built binary and loading the manifest it wrote.

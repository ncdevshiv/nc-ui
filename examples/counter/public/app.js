// The page's half of the application.
//
// The host injects `window.ncui` before this module runs, so the bridge is already there.
// Everything here goes through it: the page never touches an engine API, and the runtime
// decides what each call means and what it costs.

const countElement = document.getElementById("count");
const statusElement = document.getElementById("status");
const incrementButton = document.getElementById("increment");
const pluginButton = document.getElementById("plugin");
const pluginElement = document.getElementById("plugin-status");
const notesButton = document.getElementById("notes");
const notesElement = document.getElementById("notes-status");
const fullscreenButton = document.getElementById("fullscreen");
const fullscreenElement = document.getElementById("fullscreen-status");
const islandElement = document.getElementById("island");
const islandMirror = document.getElementById("island-mirror");
const islandStatus = document.getElementById("island-status");
const linkElement = document.getElementById("link");
const shortcutStatus = document.getElementById("shortcut-status");

let fullscreen = false;

let count = 0;

function render() {
  countElement.textContent = String(count);
}

function describeRuntime() {
  return Promise.all([
    window.ncui.invoke("describe", {}),
    window.ncui.invoke("fs.read_text", { path: "notes.txt" }),
  ])
    .then(([description, note]) => {
      statusElement.textContent =
        `Runtime ${description.runtime_version} · api ${description.api_version} · ` +
        `${description.platform} · ${description.methods.length} methods

` +
        `notes.txt (${note.bytes} bytes): ${note.text.trim()}`;
      incrementButton.disabled = false;
    })
    .catch((error) => {
      statusElement.textContent = `The runtime refused the call: ${error.message}`;
    });
}

function increment() {
  count += 1;
  render();
  // The window says what it holds. `window_shape` names only the field it wants changed, so the
  // title follows the count while the window stays where the user put it.
  window.ncui
    .invoke("window_shape", { title: `NC-UI Counter — ${count}` })
    .catch((error) => {
      statusElement.textContent =
        `The runtime refused window_shape: ${error.code} — ${error.message}`;
    });
  // The runtime is told what happened, and its answer is shown. A page that keeps its
  // own state and tells the runtime about it is the shape every real application has.
  describeRuntime();
}

incrementButton.addEventListener("click", increment);

// A global shortcut the manifest declares. The runtime holds the key and tells the page when
// it is pressed; what the press *means* is the page's decision, which is why the event carries
// the command name the manifest gave rather than an action the runtime chose.
window.ncui.on("shortcut", (event) => {
  if (event && event.command === "increment") {
    increment();
    shortcutStatus.textContent = `Ctrl+Alt+N pressed (${event.accelerator})`;
  }
});

// The plugin is a WebAssembly component: code this page did not write, running in the
// runtime's process under a grant the runtime negotiated rather than the one the page holds.
//
// `consent: true` is required and is not a formality. Running components is a destructive
// capability — it is permission to execute code nobody here wrote — so the runtime asks on
// every call, and a call that does not state its consent is treated as withholding it.
//
// The component reads notes.txt itself, through `fs.read_text`. It cannot read the file
// directly: it declares that service in its own manifest, and the runtime serves it only
// because the negotiation granted the capability behind it.
pluginButton.addEventListener("click", () => {
  pluginButton.disabled = true;
  pluginElement.textContent = "Loading the plugin…";
  window.ncui
    .invoke("component.open", {
      name: "greeter",
      artifact: "plugins/greeter.wat",
      consent: true,
    })
    .then((opened) => {
      pluginElement.textContent =
        `Loaded ${opened.name} (ABI ${opened.abi_version}), granted: ` +
        `${opened.grant.join(", ") || "nothing"}
` +
        `Methods: ${opened.methods.join(", ")} · services: ${opened.services.join(", ")}`;
      return window.ncui.invoke("component.invoke", {
        name: "greeter",
        method: "greet",
        consent: true,
      });
    })
    .then((answer) => {
      pluginElement.textContent +=
        `

The plugin read notes.txt through the runtime:
${answer.result.text.trim()}`;
      pluginButton.disabled = false;
    })
    .catch((error) => {
      pluginElement.textContent = `The runtime refused: ${error.code} — ${error.message}`;
      pluginButton.disabled = false;
    });
});

// The notes window is opened by the page, under a name. The name is what makes it remembered:
// the runtime writes down where the window was when it closes and puts it back there next run.
// The page names it because the page is what knows that two windows it opens are two windows.
notesButton.addEventListener("click", () => {
  notesButton.disabled = true;
  notesElement.textContent = "Opening the notes window…";
  window.ncui
    .invoke("create_window", {
      url: new URL("notes.html", location.href).href,
      title: "Notes",
      remember: "notes",
    })
    .then(() => window.ncui.invoke("list_windows", {}))
    .then((answer) => {
      const notes = answer.windows.find((entry) => entry.url.endsWith("notes.html"));
      notesElement.textContent = notes
        ? `Notes is open at ${notes.x ?? "the platform's choice"}, ${notes.y ?? "—"} ` +
          `(${notes.width}×${notes.height}). Move it and restart to see it come back.`
        : "The notes window was opened but is not in the list.";
      notesButton.disabled = false;
    })
    .catch((error) => {
      notesElement.textContent = `The runtime refused create_window: ${error.code} — ${error.message}`;
      notesButton.disabled = false;
    });
});

// The page asks for its own window to be made fullscreen, and reports what the platform said.
//
// `window_state` takes exactly one change per call and answers with the state the platform
// reported afterwards, so the button's label follows the screen rather than the page's belief
// about it — press Escape while fullscreen and the next press asks for the other direction.
fullscreenButton.addEventListener("click", () => {
  fullscreenButton.disabled = true;
  window.ncui
    .invoke("window_state", { fullscreen: !fullscreen })
    .then((answer) => {
      fullscreen = answer.state.fullscreen;
      fullscreenButton.textContent = fullscreen ? "Leave fullscreen" : "Go fullscreen";
      fullscreenElement.textContent =
        `The platform reports this window fullscreen: ${fullscreen}, ` +
        `maximised: ${answer.state.maximized}, focused: ${answer.state.focused}.`;
      fullscreenButton.disabled = false;
    })
    .catch((error) => {
      fullscreenElement.textContent =
        `The runtime refused window_state: ${error.code} — ${error.message}`;
      fullscreenButton.disabled = false;
    });
});

// The island: a region of this page whose pixels the GPU produced.
//
// The runtime serves a rendered frame on this page's own origin, so the fetch below is an ordinary
// same-origin GET — no custom URL scheme, and no second server to trust. The bytes are raw RGBA, so
// the page wraps them in an `ImageData` and draws them into a canvas; from there the browser
// composites the island with the rest of the document.
//
// The island's content is rows of *text*: the runtime shapes each line with the machine's own font,
// rasterises the glyphs into coverage and composites them over the row, so this canvas shows type
// the GPU drew rather than a coloured rectangle.
const islandContext = islandElement.getContext("2d");
const islandWidth = islandElement.width;
const islandHeight = islandElement.height;
const islandRows = ["First row", "Second row", "Third row"];
const islandRowHeight = 26;
const islandRowQuery = islandRows
  .map(
    (text) =>
      `${islandRowHeight}:1a2233ff:e8eefaff:16:${encodeURIComponent(text)}`,
  )
  .join(",");
const islandQuery =
  `/__ncui/island?w=${islandWidth}&h=${islandHeight}&x=0&y=0` +
  `&iw=${islandWidth}&ih=${islandHeight}&rows=${islandRowQuery}`;
fetch(islandQuery)
  .then((response) => {
    if (!response.ok) {
      throw new Error(`the runtime answered ${response.status}`);
    }
    return response.arrayBuffer();
  })
  .then((buffer) => {
    const pixels = new Uint8ClampedArray(buffer);
    islandContext.putImageData(new ImageData(pixels, islandWidth, islandHeight), 0, 0);
    islandStatus.textContent =
      `The runtime shaped, rasterised and drew ${islandRows.length} rows of type into ` +
      `${islandWidth}×${islandHeight} pixels on the GPU, and this page composited them. Move the ` +
      `window over another one: the island moves with the page, because the browser is compositing ` +
      `it, not the runtime.`;
  })
  .catch((error) => {
    islandStatus.textContent = `No island on this machine: ${error.message}`;
  });

// A canvas tells a screen reader nothing, and the page owns the DOM — so the page asks the runtime
// for the island's accessible description and mirrors it into a list a reader can walk. This is the
// page's half of making a GPU region reachable; the runtime's half is serving the description from
// the same model it draws from, so the two cannot disagree.
fetch(`${islandQuery}&a11y=1&label=${encodeURIComponent("Island rows")}`)
  .then((response) => {
    if (!response.ok) {
      throw new Error(`the runtime answered ${response.status}`);
    }
    return response.json();
  })
  .then((description) => {
    islandMirror.setAttribute("aria-label", description.label);
    islandMirror.replaceChildren(
      ...description.rows.map((row) => {
        const item = document.createElement("li");
        item.textContent = row.text;
        return item;
      }),
    );
  })
  .catch(() => {
    // A machine that cannot render an island has no description to mirror, and the status line above
    // already says so. Nothing is announced rather than something invented.
    islandMirror.replaceChildren();
  });

// The link this application was opened with, if it was opened by one. A deep link arrives as an
// argument, and the page is the application it was addressed to — so it can ask, and it gets an
// answer either way: "opened without a link" is a fact rather than a failure.
window.ncui
  .invoke("app.link", {})
  .then((answer) => {
    linkElement.textContent = answer.opened_by_link
      ? `Opened by ${answer.link}`
      : "Opened without a link. Run the register-protocol script in the bundle, then click a " +
        "ncui-counter:// link to open this application with one.";
  })
  .catch((error) => {
    linkElement.textContent = `The runtime refused app.link: ${error.message}`;
  });

render();
describeRuntime();

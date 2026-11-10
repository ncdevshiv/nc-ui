// The page's half of the port.
//
// The original Tauri application's whole behaviour is: a page presses a button, a native
// command runs, and the page shows what came back. On NC-UI the native command is a component
// the page did not write, loaded under `run.components` and invoked through the component host.
// The page also measures the round trip when the benchmark harness leaves a `bench-config.json`
// beside the application.

const WORD = "You triggered the tauri action!";

const wordElement = document.getElementById("word");
const triggerButton = document.getElementById("trigger");
const status = document.getElementById("status");
const benchElement = document.getElementById("bench");

let opening = null;

function openComponent() {
  if (opening === null) {
    opening = window.ncui
      .invoke("component.open", {
        name: "word_signal",
        artifact: "plugins/word_signal.wat",
        consent: true,
      })
      .catch((error) => {
        opening = null;
        throw error;
      });
  }
  return opening;
}

function invokeReturnString(payload) {
  return openComponent().then(() =>
    window.ncui.invoke("component.invoke", {
      name: "word_signal",
      method: "return_string",
      payload,
      consent: true,
    }),
  );
}

function triggerAction() {
  triggerButton.disabled = true;
  status.textContent = "Asking the native side…";
  invokeReturnString({ word: WORD })
    .then((answer) => {
      const word = answer.result?.word;
      wordElement.textContent = word ?? JSON.stringify(answer.result);
      status.textContent =
        `The native command answered (component call ${answer.calls}, ` +
        `fuel ${answer.fuel_used}).`;
      triggerButton.disabled = false;
    })
    .catch((error) => {
      status.textContent = `The runtime refused: ${error.code} — ${error.message}`;
      triggerButton.disabled = false;
    });
}

triggerButton.addEventListener("click", triggerAction);

// The harness writes `bench-config.json` into the application directory before launching.
// Its absence is the normal case, not an error.
function readBenchConfig() {
  return window.ncui
    .invoke("fs.read_text", { path: "bench-config.json" })
    .then((note) => JSON.parse(note.text))
    .catch(() => null);
}

function runBench(config) {
  const rounds = Number(config.rounds ?? 200);
  const output = config.output ?? "bench-result.json";
  // The component is the port's native command. Loading it first means the
  // benchmark does not report a result if the component cannot be loaded.
  return openComponent().then(() => {
    const started = performance.now();
    let completed = 0;

    const step = (index) => {
      if (index >= rounds) {
        const elapsed = performance.now() - started;
        return Promise.resolve({ rounds, completed, elapsed_ms: elapsed });
      }
      return window.ncui
        .invoke("list_windows", {})
        .then(() => {
          completed += 1;
          return step(index + 1);
        })
        .catch(() => step(index + 1));
    };

    return step(0);
  }).then((result) => {
    result.per_round_ms = result.elapsed_ms / rounds;
    result.framework = "ncui";
    const text = JSON.stringify(result);
    benchElement.textContent = text;
    return window.ncui
      .invoke("fs.write_text", { path: output, text, consent: true })
      .then(() => result)
      .catch((error) => {
        benchElement.textContent = `${text}\n(write refused: ${error.message})`;
        return result;
      });
  });
}

readBenchConfig().then((config) => {
  if (config) {
    return runBench(config);
  }
  return null;
});

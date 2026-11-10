// The page's half of the port.
//
// The host injects `window.ncui` before this module runs, so the bridge is already there.
// The page asks the runtime who it is — the same question the original asked Electron's
// preload — and, when the benchmark harness has left a `bench-config.json` beside the
// application, measures the round trip of the bridge and writes the answer back.

const runtimeVersion = document.getElementById("runtime-version");
const apiVersion = document.getElementById("api-version");
const platform = document.getElementById("platform");
const methodCount = document.getElementById("method-count");
const status = document.getElementById("status");
const benchElement = document.getElementById("bench");

function describeRuntime() {
  return window.ncui
    .invoke("describe", {})
    .then((description) => {
      runtimeVersion.textContent = description.runtime_version;
      apiVersion.textContent = description.api_version;
      platform.textContent = description.platform;
      methodCount.textContent = String(description.methods.length);
      status.textContent =
        `The runtime answered: ${description.methods.length} methods, ` +
        `platform ${description.platform}.`;
      return description;
    })
    .catch((error) => {
      status.textContent = `The runtime refused the call: ${error.message}`;
      throw error;
    });
}

// The harness writes `bench-config.json` into the application directory before launching.
// Its absence is the normal case, not an error: the page then just describes the runtime.
function readBenchConfig() {
  return window.ncui
    .invoke("fs.read_text", { path: "bench-config.json" })
    .then((note) => JSON.parse(note.text))
    .catch(() => null);
}

function runBench(config) {
  const rounds = Number(config.rounds ?? 200);
  const output = config.output ?? "bench-result.json";
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

  return step(0).then((result) => {
    result.per_round_ms = result.elapsed_ms / rounds;
    result.runtime_version = runtimeVersion.textContent;
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

describeRuntime()
  .catch(() => null)
  .then(readBenchConfig)
  .then((config) => {
    if (config) {
      return runBench(config);
    }
    return null;
  });

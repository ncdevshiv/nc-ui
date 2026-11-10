/**
 * This file is loaded via the <script> tag in the index.html file and will
 * be executed in the renderer process for that window. No Node.js APIs are
 * available in this process because `nodeIntegration` is turned off and
 * `contextIsolation` is turned on. The benchmark bridge the preload exposes
 * is the only way it reaches the main process.
 *
 * When the main process loaded this page with a `bench` query, run IPC round
 * trips and write the measurement back.
 */
const params = new URLSearchParams(location.search)
const benchOutput = params.get('bench')

if (benchOutput && window.bench) {
  const rounds = Number(params.get('rounds') || '200')
  const started = performance.now()
  let completed = 0

  const step = (index) => {
    if (index >= rounds) {
      const elapsed = performance.now() - started
      return Promise.resolve({
        rounds,
        completed,
        elapsed_ms: elapsed,
        per_round_ms: elapsed / rounds,
        framework: 'electron',
      })
    }
    return window.bench
      .invoke('echo', 'x')
      .then(() => {
        completed += 1
        return step(index + 1)
      })
      .catch(() => step(index + 1))
  }

  step(0).then((result) => window.bench.invoke('write-bench', JSON.stringify(result)))
}

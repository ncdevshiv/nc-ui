// Tauri 2 moved `invoke` from the `tauri` module to `core`.
import { invoke } from '@tauri-apps/api/core'
import {createSignal, onMount} from "solid-js"

function App() {

  const [word, setWord] = createSignal("")
  const [bench, setBench] = createSignal("")

  const handleClick = async (event) => {
    // invoke backend action and save result in variable
    const result = await invoke("return_string", {
          word: "You triggered the tauri action!"
        })

    // alert the return value
    setWord(result)
  }

  // The comparison harness sets NCUI_BENCH_OUTPUT in the environment. The main process
  // exposes the path through `bench_config`; when it is present, run IPC round trips and
  // write the measurement back.
  onMount(async () => {
    const output = await invoke("bench_config")
    if (!output) return
    const rounds = Number(await invoke("bench_rounds")) || 200
    const started = performance.now()
    let completed = 0
    for (let i = 0; i < rounds; i += 1) {
      await invoke("return_string", { word: "You triggered the tauri action!" })
      completed += 1
    }
    const elapsed = performance.now() - started
    const result = {
      rounds,
      completed,
      elapsed_ms: elapsed,
      per_round_ms: elapsed / rounds,
      framework: "tauri",
    }
    setBench(JSON.stringify(result))
    await invoke("write_bench", { json: JSON.stringify(result) })
  })

  return (
    <div>
      <h1>Word signal is currently: {word}</h1>
      <button onClick={handleClick}>Trigger the Tauri Action</button>
      <p>{bench()}</p>
    </div>
  );
}

export default App;

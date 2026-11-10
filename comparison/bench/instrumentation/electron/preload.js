/**
 * The preload script runs before `index.html` is loaded in the renderer. It fills the version
 * labels the page shows and exposes the benchmark bridge over the context bridge.
 *
 * https://www.electronjs.org/docs/latest/tutorial/sandbox
 */
const { contextBridge, ipcRenderer } = require('electron')

window.addEventListener('DOMContentLoaded', () => {
  const replaceText = (selector, text) => {
    const element = document.getElementById(selector)
    if (element) element.innerText = text
  }

  for (const type of ['chrome', 'node', 'electron']) {
    replaceText(`${type}-version`, process.versions[type])
  }
})

contextBridge.exposeInMainWorld('bench', {
  invoke: (channel, ...args) => ipcRenderer.invoke(channel, ...args),
})

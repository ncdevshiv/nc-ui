// Modules to control application life and create native browser window
const { app, BrowserWindow, ipcMain } = require('electron')
const fs = require('node:fs')
const path = require('node:path')

// The comparison harness sets NCUI_BENCH_OUTPUT to a file path. When it is set, the app
// exposes an echo IPC and a writer and loads the page with the output path in the query, so
// the renderer can measure IPC round trips and write the result back.
const benchOutput = process.env.NCUI_BENCH_OUTPUT
const benchRounds = process.env.NCUI_BENCH_ROUNDS || '200'

if (benchOutput) {
  ipcMain.handle('echo', (_event, text) => text)
  ipcMain.handle('write-bench', (_event, json) => {
    fs.writeFileSync(benchOutput, json)
  })
}

function createWindow () {
  // Create the browser window.
  const mainWindow = new BrowserWindow({
    width: 800,
    height: 600,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js')
    }
  })

  // and load the index.html of the app.
  mainWindow.loadFile('index.html', {
    query: benchOutput ? { bench: benchOutput, rounds: benchRounds } : undefined,
  })

  // Open the DevTools.
  // mainWindow.webContents.openDevTools()
}

// This method will be called when Electron has finished
// initialization and is ready to create browser windows.
// Some APIs can only be used after this event occurs.
app.whenReady().then(() => {
  createWindow()

  app.on('activate', function () {
    // On macOS it's common to re-create a window in the app when the
    // dock icon is clicked and there are no other windows open.
    if (BrowserWindow.getAllWindows().length === 0) createWindow()
  })
})

// Quit when all windows are closed, except on macOS. On there, it's common
// for applications to keep their menu bar active until the user quits
// explicitly with Cmd + Q.
app.on('window-all-closed', function () {
  if (process.platform !== 'darwin') app.quit()
})

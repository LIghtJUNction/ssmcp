pragma Singleton
import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root

  readonly property string template: "ssmcp username@server --control-only"
  readonly property string runtime: Quickshell.env("XDG_RUNTIME_DIR")
  readonly property url launcher: Qt.resolvedUrl("launch.sh")
  property Item terminal: null
  property bool running: false
  property bool stopping: false
  property string runDirectory: ""
  property string errorKey: ""
  readonly property var count: {
    if (!running) return null
    try {
      const calls = JSON.parse(stats.text()).tool_calls_10m
      if (Number.isSafeInteger(calls) && calls >= 0) return calls
    } catch (_) {}
    return null
  }

  function valid(cwd, command) {
    return /^(\/|~(\/|$))/.test(cwd)
      && /(?:^|[\s'"])[a-zA-Z0-9_.-]+@[^\s'";]+(?=$|[\s'"])/.test(command)
      && !/(?:^|[\s'"])username@server(?=$|[\s'"])/.test(command)
  }

  function open(host, cwd, command) {
    if (running) {
      terminal.parent = host
      Qt.callLater(() => terminal?.forceActiveFocus())
      return
    }
    if (!valid(cwd, command)) return
    if (!runtime) {
      errorKey = "runtimeError"
      return
    }
    const component = Qt.createComponent("EmbeddedTerminal.qml", Component.PreferSynchronous)
    if (component.status !== Component.Ready) {
      errorKey = "terminalError"
      console.warn(component.errorString())
      return
    }
    if (terminal) terminal.destroy()
    const nextDirectory = runtime + "/ssmcp-omarchy-" + Quickshell.processId + "-" + Date.now()
    const script = decodeURIComponent(String(launcher).replace(/^file:\/\//, ""))
    terminal = component.createObject(root, {
      parent: host,
      launchArgs: [script, nextDirectory, encodeURIComponent(cwd), encodeURIComponent(command)]
    })
    if (!terminal) {
      errorKey = "terminalError"
      return
    }
    errorKey = ""
    runDirectory = nextDirectory
    running = true
    terminal.ended.connect(root.finished)
    terminal.start()
    Qt.callLater(() => terminal?.forceActiveFocus())
  }

  function stop() {
    if (running && !stopping) stopping = terminal.stop()
  }

  function finished() {
    running = false
    stopping = false
    runDirectory = ""
    errorKey = "ended"
  }

  FileView {
    id: stats
    path: root.runDirectory ? root.runDirectory + "/calls.json" : ""
    printErrors: false
  }

  Timer {
    running: root.running
    repeat: true
    interval: 1000
    onTriggered: stats.reload()
  }
}

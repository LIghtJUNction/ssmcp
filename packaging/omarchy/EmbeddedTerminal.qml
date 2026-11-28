import QtQuick
import QMLTermWidget as Term
import qs.Commons as Commons

Term.QMLTermWidget {
  id: root

  required property list<string> launchArgs
  signal ended()

  anchors.fill: parent
  clip: true
  font: Qt.font({family: "monospace", pixelSize: Commons.Style.font.body})
  enableBold: true
  colorScheme: "WhiteOnBlack"

  session: Term.QMLTermSession {
    id: pty
    shellProgram: "/usr/bin/bash"
    shellProgramArgs: root.launchArgs
    onFinished: root.ended()
  }

  function start() { pty.startShellProgram() }
  function stop() { return pty.sendSignal(15) }

  Keys.onPressed: function(event) {
    if (!(event.modifiers & Qt.ControlModifier) || !(event.modifiers & Qt.ShiftModifier)) return
    if (event.key === Qt.Key_C) root.copyClipboard()
    else if (event.key === Qt.Key_V) root.pasteClipboard()
    else return
    event.accepted = true
  }
}

import QtQuick
import QtQuick.Layouts
import qs.Commons as Commons
import qs.Ui as Ui

Ui.BarWidget {
  id: root
  moduleName: "io.github.lightjunction.ssmcp"

  readonly property string cwd: setting("cwd", "")
  readonly property string command: setting("command", "")
  readonly property bool configured: Session.valid(cwd, command)
  readonly property string calls: Session.count === null ? "—" : Commons.I18n.formatNumber(Session.count, 0)
  readonly property bool opened: settingsPopup.open || terminalPopup.open
  property bool saveFailed: false

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function open() {
    if (!configured && !Session.running) {
      openSettings()
      return
    }
    settingsPopup.open = false
    terminalPopup.open = true
    Session.open(terminalHost, cwd, command)
  }

  function close() {
    settingsPopup.open = false
    terminalPopup.open = false
  }

  function openSettings() {
    directory.text = cwd || "~"
    launchCommand.text = command || Session.template
    saveFailed = false
    terminalPopup.open = false
    settingsPopup.open = true
  }

  function save() {
    const entry = Object.assign({}, settings, {id: moduleName,
      cwd: directory.text.trim(), command: launchCommand.text.trim()})
    saveFailed = !bar?.shell?.updateEntryInline(moduleName, entry)
    if (!saveFailed) settings = entry
  }

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  Ui.BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.vertical && Session.count !== null ? root.calls : "󰆍"
    labelText: root.vertical ? "" : root.calls
    fixedWidth: root.vertical ? -1 : slotSize + labelMetrics.advanceWidth + Commons.Style.space(4)
    active: Session.running
    tooltipText: Session.errorKey ? strings.t(Session.errorKey)
      : Session.count !== null ? strings.t("count").arg(root.calls)
      : strings.t(Session.running ? "waiting" : root.configured ? "openHint" : "configureHint")

    TextMetrics {
      id: labelMetrics
      font.family: button.fontFamily
      font.pixelSize: button.fontSize
      text: button.labelText
    }

    onPressed: function(mouseButton) {
      if (mouseButton === Qt.RightButton) {
        if (settingsPopup.open) root.close()
        else root.openSettings()
      } else if (mouseButton === Qt.LeftButton) {
        if (root.opened) root.close()
        else root.open()
      }
    }
  }

  Ui.PopupCard {
    id: settingsPopup
    anchorItem: button
    owner: root
    bar: root.bar
    contentWidth: fittedContentWidth(Commons.Style.space(450))
    contentHeight: fittedContentHeight(form.implicitHeight)

    ColumnLayout {
      id: form
      anchors.left: parent.left
      anchors.right: parent.right
      spacing: Commons.Style.space(7)

      readonly property bool changed: directory.text.trim() !== root.cwd || launchCommand.text.trim() !== root.command

      Text {
        text: strings.t("cwd")
        color: Commons.Color.foreground
        font.pixelSize: Commons.Style.font.body
      }

      Ui.TextField {
        id: directory
        Layout.fillWidth: true
        selectByMouse: true
      }

      Text {
        text: strings.t("command")
        color: Commons.Color.foreground
        font.pixelSize: Commons.Style.font.body
      }

      Ui.TextField {
        id: launchCommand
        Layout.fillWidth: true
        selectByMouse: true
      }

      Text {
        Layout.fillWidth: true
        visible: text !== ""
        text: root.saveFailed ? strings.t("saveError") : Session.errorKey ? strings.t(Session.errorKey)
          : !root.configured ? strings.t("setupHelp") : Session.running && form.changed ? strings.t("nextLaunch") : ""
        wrapMode: Text.Wrap
        color: Commons.Color.foreground
        font.pixelSize: Commons.Style.font.body
      }

      RowLayout {
        Layout.alignment: Qt.AlignRight
        spacing: Commons.Style.space(7)

        Ui.Button {
          text: strings.t("save")
          bordered: true
          focusable: true
          enabled: form.changed && Session.valid(directory.text.trim(), launchCommand.text.trim())
          opacity: enabled ? 1 : 0.5
          onClicked: root.save()
        }

        Ui.Button {
          text: strings.t(Session.running ? "close" : "open")
          bordered: true
          focusable: true
          enabled: Session.running || (root.configured && !form.changed)
          opacity: enabled ? 1 : 0.5
          onClicked: {
            if (Session.running) Session.stop()
            else root.open()
          }
        }
      }
    }
  }

  Ui.PopupCard {
    id: terminalPopup
    anchorItem: button
    owner: root
    bar: root.bar
    contentWidth: fittedContentWidth(Commons.Style.space(920))
    contentHeight: cappedContentHeight(Commons.Style.space(480))

    ColumnLayout {
      anchors.fill: parent
      spacing: Commons.Style.space(5)

      Item {
        id: terminalHost
        Layout.fillWidth: true
        Layout.fillHeight: true
        clip: true

        Text {
          anchors.centerIn: parent
          width: parent.width
          visible: !Session.terminal && Session.errorKey !== ""
          text: strings.t(Session.errorKey)
          wrapMode: Text.Wrap
          horizontalAlignment: Text.AlignHCenter
          color: Commons.Color.foreground
          font.pixelSize: Commons.Style.font.body
        }
      }

      RowLayout {
        Layout.alignment: Qt.AlignRight
        spacing: Commons.Style.space(7)

        Ui.Button {
          text: strings.t("collapse")
          focusable: true
          onClicked: root.close()
        }

        Ui.Button {
          text: strings.t(Session.stopping ? "closing" : Session.running ? "close" : "open")
          bordered: true
          focusable: true
          enabled: !Session.stopping
          opacity: enabled ? 1 : 0.5
          onClicked: Session.running ? Session.stop() : root.open()
        }
      }
    }
  }
}

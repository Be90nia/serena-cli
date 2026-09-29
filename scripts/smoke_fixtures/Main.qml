import QtQuick

Rectangle {
    id: root
    width: 320
    height: 240

    property string smokeTitle: "smoke"

    Text {
        anchors.centerIn: parent
        text: root.smokeTitle
    }
}

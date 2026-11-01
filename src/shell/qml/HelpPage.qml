import QtQuick
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.ki18n

FormCard.FormCardPage {
    id: page

    required property var core

    title: KI18n.i18n("Help")

    // Same padding on every page: cards float with rounded corners instead of touching the window edges.
    topPadding: Kirigami.Units.largeSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    bottomPadding: Kirigami.Units.largeSpacing

    component Entry: FormCard.FormTextDelegate {
    }

    SectionTitle {
        first: true
        text: KI18n.i18n("Getting Started")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "document-send"
            text: KI18n.i18n("Send")
            description: KI18n.i18n("Choose or drop files, or type some text, on the Send page, then pick a device. On the other device, open Quick Share.")
        }
        Entry {
            icon.name: "visibility"
            text: KI18n.i18n("Be Found")
            description: KI18n.i18n("Keep “Visible to nearby devices” on in Preferences. The other device must be set to Everyone to be found, and Android sets it back after 10 minutes.")
        }
        Entry {
            icon.name: "folder-download-symbolic"
            text: KI18n.i18n("Receive")
            description: KI18n.i18n("An incoming transfer asks for your approval in a notification. Received files go to the download folder chosen in Preferences.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Paired and Trusted Devices")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "network-connect"
            text: KI18n.i18n("Paired Devices")
            description: KI18n.i18n("Two systems running KuickShare can pair with an 8-digit code, then send to each other without asking. The port 47891 must be reachable.")
        }
        Entry {
            icon.name: "security-high"
            text: KI18n.i18n("Trusted Devices")
            description: KI18n.i18n("“Accept and Trust” on a notification accepts a device without asking from then on. It is recognised by name and network address, so only on the same network, never for transfers over Bluetooth, which always ask. Another device on the same network can copy both, so use it only where you trust every device.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Wi-Fi Direct (Experimental)")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "network-wireless"
            text: KI18n.i18n("Without a Shared Network")
            description: KI18n.i18n("The transfer starts over Bluetooth and moves to a temporary Wi-Fi link: this system hosts a hotspot to receive, and joins the other device’s Wi-Fi Direct group to send. Turn it on in Preferences; it needs NetworkManager, its dnsmasq package and Bluetooth.")
        }
        Entry {
            icon.name: "smartphone-symbolic"
            text: KI18n.i18n("On Android")
            description: KI18n.i18n("Keep Quick Share’s Receive screen open and set to Everyone. A device on the same network as this system is reached over the network instead.")
        }
        Entry {
            icon.name: "security-medium"
            text: KI18n.i18n("Wi-Fi Connection and Firewall")
            description: KI18n.i18n("With one Wi-Fi card, the normal connection of this system may drop while the temporary link is active. Without firewalld, the device that joins the hotspot can reach every service on this system.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Troubleshooting")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "view-hidden"
            text: KI18n.i18n("The other device does not see this system")
            description: KI18n.i18n("Check that “Visible to nearby devices” is on, that Do Not Disturb is off (it hides this system) and that both are on the same network.")
        }
        Entry {
            icon.name: "edit-find"
            text: KI18n.i18n("This system does not see the other device")
            description: KI18n.i18n("Set the other device to Everyone and keep its Quick Share screen open.")
        }
        Entry {
            icon.name: "network-bluetooth"
            text: KI18n.i18n("The other device is not found over Bluetooth")
            description: KI18n.i18n("Open Quick Share’s Receive screen on the other device, set it to Everyone and use “Search Again” on the Send page.")
        }
        Entry {
            icon.name: "security-medium"
            text: KI18n.i18n("Firewall")
            description: KI18n.i18n("Nearby devices are found through announcements on the local network (mDNS). The transfer port is random unless you fix one in Preferences: open that port in the firewall.")
        }
    }
}

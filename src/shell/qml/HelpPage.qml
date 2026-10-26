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
        text: KI18n.i18n("Getting started")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "document-send"
            text: KI18n.i18n("Send")
            description: KI18n.i18n("Choose or drop files, or type some text, on the Send page, then pick a device. On the phone, open Quick Share.")
        }
        Entry {
            icon.name: "visibility"
            text: KI18n.i18n("Be found")
            description: KI18n.i18n("Keep “Visible to nearby devices” on in Preferences. The phone must be set to Everyone to be found, and Android sets it back after 10 minutes.")
        }
        Entry {
            icon.name: "folder-download-symbolic"
            text: KI18n.i18n("Receive")
            description: KI18n.i18n("An incoming transfer asks for your approval in a notification. Received files go to the download folder chosen in Preferences.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Paired and trusted devices")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "security-high"
            text: KI18n.i18n("Paired computers")
            description: KI18n.i18n("Two computers running KuickShare can pair with an 8-digit code, then send to each other without asking. The port 47891 must be reachable.")
        }
        Entry {
            icon.name: "security-high"
            text: KI18n.i18n("Trusted devices")
            description: KI18n.i18n("“Accept and trust” on a notification accepts a phone without asking from then on. It is recognised by name and MAC address, so only on the same network, never for transfers over Bluetooth, which always ask.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Wi-Fi Direct (experimental)")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "network-wireless"
            text: KI18n.i18n("Without a shared network")
            description: KI18n.i18n("The transfer starts over Bluetooth and moves to a temporary Wi-Fi link: this computer hosts a hotspot to receive, and joins the phone's Wi-Fi Direct group to send. Turn it on in Preferences; it needs NetworkManager, its dnsmasq package and Bluetooth.")
        }
        Entry {
            icon.name: "smartphone-symbolic"
            text: KI18n.i18n("On the phone")
            description: KI18n.i18n("Keep Quick Share's Receive screen open and set to Everyone. A phone on the same network as this computer is reached over the network instead.")
        }
        Entry {
            icon.name: "network-wireless"
            text: KI18n.i18n("Wi-Fi connection and firewall")
            description: KI18n.i18n("With one Wi-Fi card, the normal connection of this computer may drop while the temporary link is active. Without firewalld, the phone that joins the hotspot can reach every service on this computer.")
        }
    }

    SectionTitle {
        text: KI18n.i18n("Troubleshooting")
    }
    FormCard.FormCard {
        maximumWidth: width - 1
        Entry {
            icon.name: "help-hint"
            text: KI18n.i18n("The phone does not see this computer")
            description: KI18n.i18n("Check that “Visible to nearby devices” is on, that Do Not Disturb is off (it hides this computer) and that both are on the same network.")
        }
        Entry {
            icon.name: "help-hint"
            text: KI18n.i18n("This computer does not see the phone")
            description: KI18n.i18n("Set the phone to Everyone and keep its Quick Share screen open.")
        }
        Entry {
            icon.name: "help-hint"
            text: KI18n.i18n("The phone is not found over Bluetooth")
            description: KI18n.i18n("Open Quick Share's Receive screen on the phone, set it to Everyone and use “Search again” on the Send page.")
        }
        Entry {
            icon.name: "help-hint"
            text: KI18n.i18n("Firewall")
            description: KI18n.i18n("Discovery uses mDNS. The transfer port is random unless you fix one in Preferences: open that port in the firewall.")
        }
    }
}

# Security

KuickShare accepts files and connections from the local network, so security reports are welcome.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private reporting: **Security → Report a vulnerability** on the
[repository](https://github.com/the-beper/KuickShare/security/advisories/new). Include what you did, what happened and the
version (Help → About). I will answer as soon as I can; this is a personal project, so there is no fixed response time.

Only the latest release is supported.

## What the application does about untrusted senders

- A transfer from an unknown sender always asks for approval in a notification, and is declined after five minutes without an answer. At most 32 requests wait at once (4 per address); the rest are declined.
- Each request is answered once, and a prompt that outlived its transfer cannot answer a new one.
- A transfer whose file names are not plain names (empty, `.`, `..`, containing `/`, `\`, control or invisible direction characters, or longer than 255 bytes) is declined before it can be accepted.
- An offer is bounded before you are asked: at most 10,000 files, sane sizes, and it must fit in the free space with a reserve of 32 MiB. Nothing is written before you accept; files never overwrite existing ones; a file that is not complete is removed, and a transfer counts as finished only when every file has its declared size. Limits: [architecture.md](docs/architecture.md#receive-limits).
- A session that stalls is ended (90 s for the handshake, 60 s without data), and the LAN listener holds at most 32 sessions, 4 per address.
- The computer's Wi-Fi hotspot (experimental) opens only its one port in the firewall. If the firewall cannot do that, the hotspot is not started and the transfer stays on Bluetooth, unless you allowed an open firewall zone for the session.
- The trust channel (TCP port 47891) is rate limited per address and capped in the number of open connections.
- Pairing needs both users to confirm the same 8-digit code, and a pairing window that the user opens.

## Known limits

These come from the Quick Share protocol, from the library used for it (`rqs_lib`) or from the design, and are not fixed:

- **Senders are not authenticated.** The device name is chosen by the sender. A device in *Trusted devices* is recognised by name and the MAC address read when its request arrived, so someone on the same network who copies both would be accepted. The trusted devices of older versions (name and address only) were deleted once for this reason, with a notice; the feature stays. Use trusted devices only on networks you trust.
- The announcement from a paired computer is matched by IP address for 60 seconds; `Unpair` voids an unused one.
- The consent token is not a secret: any program of your user on the session bus can read it, call `Accept` and allow the open firewall zone (`RequestWifiUpgrade`). The bus is the trust boundary.
- A crash, SIGKILL or power loss while a file is being received can leave the partial file in the download folder. Cancel, decline, errors, disconnects and timeouts remove it.
- A sender you accept can use the free space of the download folder down to the reserve of 32 MiB.
- Many addresses can hold the 32 LAN session slots until the handshake and idle limits end them, and each session can hold about 10 MiB of memory before you answer.
- With your consent for a session, the hotspot of the Wi-Fi upgrade can run in firewalld's `trusted` zone (every port open to the phone that joins), or, without firewalld, in the zone NetworkManager picks, where another firewall may leave ports open or block the phone. Cleanup of the hotspot at stop is bounded at 2 s, so a hotspot or an open firewall port can remain after a timeout or a crash until the next start. None of this was hand-tested with a phone.
- Discovery uses mDNS and a fixed or random TCP port, which must be reachable on the local network.

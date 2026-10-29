use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::DeviceType;
use crate::hdl::new_daemon;
use crate::utils::{is_not_self_ip, parse_mdns_endpoint_info};

/// How long the reachability probe of a resolved service may take.
const RESOLVE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Default)]
pub struct EndpointInfo {
    pub fullname: String,
    pub id: String,
    pub name: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    pub rtype: Option<DeviceType>,
    pub present: Option<bool>,
    /// Set for a phone found over Bluetooth LE; `id` is then `BleTarget::id()`.
    pub ble: Option<BleTarget>,
}

/// A phone reachable over BLE. Only the endpoint id is stable: the address and the L2CAP PSM
/// of its advertisement change at every scan, so a send has to scan again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BleTarget {
    pub endpoint_id: [u8; 4],
}

impl BleTarget {
    const PREFIX: &'static str = "ble:";

    /// `ble:` and the endpoint id as text; hex (8 characters) when its bytes are not all
    /// printable ASCII, which an id of 4 characters can never be mistaken for.
    pub fn id(&self) -> String {
        if self.endpoint_id.iter().all(u8::is_ascii_graphic) {
            let text: String = self.endpoint_id.iter().map(|b| char::from(*b)).collect();
            format!("{}{text}", Self::PREFIX)
        } else {
            format!("{}{}", Self::PREFIX, hex::encode(self.endpoint_id))
        }
    }

    /// Inverse of [`id`](Self::id); `None` for anything else, a TCP `ip:port` included.
    pub fn parse(id: &str) -> Option<Self> {
        let rest = id.strip_prefix(Self::PREFIX)?;
        let endpoint_id = match rest.len() {
            4 if rest.bytes().all(|b| b.is_ascii_graphic()) => rest.as_bytes().try_into().ok()?,
            8 => hex::decode(rest).ok()?.try_into().ok()?,
            _ => return None,
        };
        Some(Self { endpoint_id })
    }
}

pub struct MDnsDiscovery {
    daemon: ServiceDaemon,
    sender: broadcast::Sender<EndpointInfo>,
}

impl MDnsDiscovery {
    pub fn new(sender: broadcast::Sender<EndpointInfo>) -> Result<Self, anyhow::Error> {
        let daemon = new_daemon()?;

        Ok(Self { daemon, sender })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("MDnsDiscovery: service starting");

        let service_type = "_FC9F5ED42C8A._tcp.local.";
        let receiver = self.daemon.browse(service_type)?;

        // Map with fullname as key and EndpointInfo as value
        let mut cache: HashMap<String, EndpointInfo> = HashMap::new();

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("MDnsDiscovery: tracker cancelled, breaking");
                    break;
                }
                r = receiver.recv_async() => {
                    match r {
                        Ok(event) => {
                            match event {
                                ServiceEvent::ServiceResolved(info) => {
                                    let port = info.get_port();

                                    let ip_hash = info.get_addresses_v4();
                                    if ip_hash.is_empty() {
                                        continue;
                                    }

                                    let ip = match ip_hash.iter().next() {
                                        Some(i) => i,
                                        None => continue,
                                    };

                                    // Check that the IP is not a "self IP"
                                    if !is_not_self_ip(ip) {
                                        continue;
                                    }

                                    // Decode the "n" text properties
                                    let n = match info.get_property("n") {
                                        Some(_n) => _n,
                                        None => continue,
                                    };

                                    // Parse the endpoint info
                                    let (dt, dn) = match parse_mdns_endpoint_info(n.val_str()) {
                                        Ok(r) => r,
                                        Err(_) => continue
                                    };

                                    let ip_port = format!("{ip}:{port}");
                                    let fullname = info.get_fullname().to_string();
                                    // The probe takes up to `RESOLVE_PROBE_TIMEOUT`: a stop does not wait for it.
                                    let reachable = tokio::select! {
                                        _ = ctk.cancelled() => {
                                            info!("MDnsDiscovery: tracker cancelled during a probe, breaking");
                                            break;
                                        }
                                        probed = tokio::time::timeout(RESOLVE_PROBE_TIMEOUT, TcpStream::connect(&ip_port)) => {
                                            matches!(probed, Ok(Ok(_)))
                                        }
                                    };
                                    if reachable {
                                        let ei = EndpointInfo {
                                            fullname: fullname.clone(),
                                            id: ip_port,
                                            name: Some(dn),
                                            ip: Some(ip.to_string()),
                                            port: Some(port.to_string()),
                                            rtype: Some(dt),
                                            present: Some(true),
                                            ble: None,
                                        };
                                        info!("ServiceResolved: Resolved a new service: {:?}", ei);
                                        cache.insert(fullname.clone(), ei.clone());
                                        let _ = self.sender.send(ei);
                                    }
                                }
                                ServiceEvent::ServiceRemoved(_, fullname) => {
                                    trace!("ServiceRemoved: checking if should remove {}", fullname);
                                    // Only remove if it has not been seen in the last cleanup_threshold
                                    let should_remove = cache.get(&fullname).map(|ei| ei.id.clone());

                                    if let Some(id) = should_remove {
                                        info!("ServiceRemoved: Remove a previous service: {}", fullname);
                                        cache.remove(&fullname);
                                        let _ = self.sender.send(EndpointInfo {
                                            id,
                                            ..Default::default()
                                        });
                                    }
                                }
                                ServiceEvent::SearchStarted(_) | ServiceEvent::SearchStopped(_) => {}
                                _ => {}
                            }
                        },
                        Err(err) => error!("MDnsDiscovery: error: {}", err),
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_ids_are_text_and_others_hex() {
        let t = BleTarget {
            endpoint_id: *b"G483",
        };
        assert_eq!(t.id(), "ble:G483");
        assert_eq!(BleTarget::parse("ble:G483"), Some(t));
        let raw = BleTarget {
            endpoint_id: [0, 1, 0xff, b'A'],
        };
        assert_eq!(raw.id(), "ble:0001ff41");
        assert_eq!(BleTarget::parse(&raw.id()), Some(raw));
        let space = BleTarget {
            endpoint_id: *b"A B\x7f",
        };
        assert_eq!(BleTarget::parse(&space.id()), Some(space));
    }

    #[test]
    fn other_ids_are_not_ble() {
        for id in [
            "192.168.1.2:4000",
            "ble:",
            "ble:ABC",
            "ble:ABCDE",
            "ble:zzzzzzzz",
            "G483",
        ] {
            assert_eq!(BleTarget::parse(id), None, "{id}");
        }
    }

    #[test]
    fn default_endpoint_is_not_ble() {
        assert_eq!(EndpointInfo::default().ble, None);
    }
}

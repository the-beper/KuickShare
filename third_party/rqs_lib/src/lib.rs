#[macro_use]
extern crate log;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::anyhow;
use channel::ChannelMessage;
#[cfg(all(feature = "experimental", target_os = "linux"))]
use hdl::BleAdvertiser;
use hdl::MDnsDiscovery;
use once_cell::sync::Lazy;
use rand::RngExt;
use rand::distr::Alphanumeric;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::hdl::MDnsServer;
use crate::manager::TcpServer;

pub mod channel;
pub mod errors;
pub mod hdl;
pub mod manager;
pub mod safety;
pub mod upgrade;
pub mod utils;

pub use hdl::{
    BleTarget, EndpointInfo, OutboundPayload, TextPayloadType, TransferState, Visibility,
};
pub use manager::SendInfo;
pub use upgrade::{WifiCredentials, WifiLink};
pub use utils::DeviceType;

pub mod sharing_nearby {
    include!(concat!(env!("OUT_DIR"), "/sharing.nearby.rs"));
}

pub mod securemessage {
    include!(concat!(env!("OUT_DIR"), "/securemessage.rs"));
}

pub mod securegcm {
    include!(concat!(env!("OUT_DIR"), "/securegcm.rs"));
}

pub mod location_nearby_connections {
    include!(concat!(env!("OUT_DIR"), "/location.nearby.connections.rs"));
}

static CUSTOM_DOWNLOAD: Lazy<RwLock<Option<PathBuf>>> = Lazy::new(|| RwLock::new(None));
static DEVICE_NAME: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new(utils::hostname()));

// None (the default) disables bandwidth upgrades entirely.
static WIFI_LINK: Lazy<RwLock<Option<Arc<dyn WifiLink>>>> = Lazy::new(|| RwLock::new(None));

// True while WIFI_LINK is set; lets the BLE receiver follow `set_wifi_link`.
static WIFI_LINK_SET: Lazy<watch::Sender<bool>> = Lazy::new(|| watch::channel(false).0);

/// The link handlers pick up when a session starts.
pub(crate) fn wifi_link() -> Option<Arc<dyn WifiLink>> {
    WIFI_LINK.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(all(feature = "experimental", target_os = "linux"))]
fn spawn_ble_scan(
    tracker: &TaskTracker,
    ctk: CancellationToken,
    sender: broadcast::Sender<EndpointInfo>,
) {
    tracker.spawn(async move {
        tokio::select! {
            () = ctk.cancelled() => {}
            outcome = hdl::ble_scan::SCANNER.scan(&sender) => debug!("BLE scan: {outcome:?}"),
        }
    });
}

#[cfg(not(all(feature = "experimental", target_os = "linux")))]
fn spawn_ble_scan(_: &TaskTracker, _: CancellationToken, _: broadcast::Sender<EndpointInfo>) {
    debug!("BLE scan not supported in this build");
}

#[derive(Debug)]
pub struct RQS {
    tracker: Option<TaskTracker>,
    ctoken: Option<CancellationToken>,
    // Discovery token is different than ctoken because he is on his own
    // - can be cancelled while the ctoken is still active
    discovery_ctk: Option<CancellationToken>,
    /// The mDNS search alone, a child of `discovery_ctk`: it can be restarted without touching the
    /// Bluetooth advertisement and scan of the same discovery.
    mdns_ctk: Option<CancellationToken>,

    // Used to trigger a change in the mDNS visibility (and later on, BLE)
    pub visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
    visibility_receiver: watch::Receiver<Visibility>,

    pub port_number: Option<u32>,

    pub message_sender: broadcast::Sender<ChannelMessage>,
}

impl Default for RQS {
    fn default() -> Self {
        Self::new(Visibility::Visible, None, None, Some(utils::hostname()))
    }
}

impl RQS {
    pub fn new(
        visibility: Visibility,
        port_number: Option<u32>,
        download_path: Option<PathBuf>,
        device_name: Option<String>,
    ) -> Self {
        {
            let mut guard = CUSTOM_DOWNLOAD.write().unwrap();
            *guard = download_path;
        }
        {
            let mut guard = DEVICE_NAME.write().unwrap();
            if let Some(device_name) = device_name {
                *guard = device_name.clone();
            }
        }

        let (message_sender, _) = broadcast::channel(50);

        // Define default visibility as per the args inside the new()
        let (visibility_sender, visibility_receiver) = watch::channel(Visibility::Invisible);
        let _ = visibility_sender.send(visibility);

        Self {
            tracker: None,
            ctoken: None,
            discovery_ctk: None,
            mdns_ctk: None,
            visibility_sender: Arc::new(Mutex::new(visibility_sender)),
            visibility_receiver,
            port_number,
            message_sender,
        }
    }

    pub async fn run(
        &mut self,
    ) -> Result<(mpsc::Sender<SendInfo>, broadcast::Receiver<()>), anyhow::Error> {
        let tracker = TaskTracker::new();
        let ctoken = CancellationToken::new();
        self.tracker = Some(tracker.clone());
        self.ctoken = Some(ctoken.clone());

        let endpoint_id: Vec<u8> = rand::rng()
            .sample_iter(Alphanumeric)
            .take(4)
            .map(u8::from)
            .collect();
        let tcp_listener =
            TcpListener::bind(format!("0.0.0.0:{}", self.port_number.unwrap_or(0))).await?;
        let binded_addr = tcp_listener.local_addr()?;
        info!("TcpListener on: {}", binded_addr);

        // So the random port can be accessed from the user if needed.
        // This does have a difference in behaviour however when port_number is Some.
        // .stop() and .run() will reuse the port number instead of generating a new one.
        self.port_number = Some(binded_addr.port() as u32);

        // MPSC for the TcpServer
        let send_channel = mpsc::channel(10);
        // Start TcpServer in own "task"
        let mut server = TcpServer::new(
            endpoint_id[..4].try_into()?,
            tcp_listener,
            self.message_sender.clone(),
            send_channel.1,
        )?;
        let ctk = ctoken.clone();
        tracker.spawn(async move { server.run(ctk).await });

        // Start MDnsServer in own "task"
        let mut mdns = MDnsServer::new(
            endpoint_id[..4].try_into()?,
            binded_addr.port(),
            self.visibility_sender.clone(),
            self.visibility_receiver.clone(),
        )?;
        let ctk = ctoken.clone();
        tracker.spawn(async move {
            if let Err(e) = mdns.run(ctk).await {
                error!("MDnsServer stopped: {}", e);
            }
        });

        // Receiving from a phone with Wi-Fi off. The advertised endpoint id has to be the mDNS one.
        #[cfg(all(feature = "experimental", target_os = "linux"))]
        hdl::ble_receiver::spawn(
            &tracker,
            ctoken.clone(),
            endpoint_id[..4].try_into()?,
            rand::rng().random(),
            self.message_sender.clone(),
            self.visibility_receiver.clone(),
            WIFI_LINK_SET.subscribe(),
        );

        tracker.close();

        // The receiver half used to report nearby senders seen by the BLE scanner, which is gone.
        // It is kept for API compatibility and only ever yields `Closed`.
        let (_, nearby_sharing) = broadcast::channel(1);
        Ok((send_channel.0, nearby_sharing))
    }

    pub fn discovery(
        &mut self,
        sender: broadcast::Sender<EndpointInfo>,
    ) -> Result<(), anyhow::Error> {
        let tracker = self
            .tracker
            .as_ref()
            .ok_or_else(|| anyhow!("The service wasn't first started"))?;

        let ctk = CancellationToken::new();
        self.discovery_ctk = Some(ctk.clone());

        #[cfg(all(feature = "experimental", target_os = "linux"))]
        {
            let ctk_blea = ctk.clone();
            tracker.spawn(async move {
                let blea = match BleAdvertiser::new().await {
                    Ok(b) => b,
                    Err(e) => {
                        error!("Couldn't init BleAdvertiser: {}", e);
                        return;
                    }
                };

                if let Err(e) = blea.run(ctk_blea).await {
                    error!("Couldn't start BleAdvertiser: {}", e);
                }
            });
        }

        spawn_ble_scan(tracker, ctk.clone(), sender.clone());

        let discovery = MDnsDiscovery::new(sender)?;
        let mdns_ctk = ctk.child_token();
        self.mdns_ctk = Some(mdns_ctk.clone());
        tracker.spawn(async move { discovery.run(mdns_ctk).await });

        Ok(())
    }

    /// Starts the mDNS search over (its queries go out at once; the library would notice a new
    /// device on its own only when it announces itself). The Bluetooth advertisement and scan of the
    /// running discovery keep going: restarting them stops the scan window in progress.
    pub fn restart_mdns_discovery(
        &mut self,
        sender: broadcast::Sender<EndpointInfo>,
    ) -> Result<(), anyhow::Error> {
        let tracker = self
            .tracker
            .as_ref()
            .ok_or_else(|| anyhow!("The service wasn't first started"))?;
        let parent = self
            .discovery_ctk
            .as_ref()
            .ok_or_else(|| anyhow!("The discovery is not running"))?;

        if let Some(old) = self.mdns_ctk.take() {
            old.cancel();
        }
        let discovery = MDnsDiscovery::new(sender)?;
        let mdns_ctk = parent.child_token();
        self.mdns_ctk = Some(mdns_ctk.clone());
        tracker.spawn(async move { discovery.run(mdns_ctk).await });
        Ok(())
    }

    /// Runs one BLE scan window in the background and reports the phones in Receive mode
    /// (`EndpointInfo::ble` set, `present` `Some(true)`, or `Some(false)` for those seen by
    /// an earlier window and not by this one) on `sender`, the channel of `discovery`.
    ///
    /// Call it when the user asks for a refresh. It does nothing (debug log only) without a
    /// Wi-Fi link, without a BlueZ adapter, while another window or a BLE send is running, and
    /// within 4 s of the previous window's start. `discovery` already runs one window.
    pub fn scan_ble(&self, sender: broadcast::Sender<EndpointInfo>) -> Result<(), anyhow::Error> {
        let (Some(tracker), Some(ctk)) = (&self.tracker, &self.ctoken) else {
            return Err(anyhow!("The service wasn't first started"));
        };
        spawn_ble_scan(tracker, ctk.clone(), sender);
        Ok(())
    }

    pub fn stop_discovery(&mut self) {
        if let Some(discovert_ctk) = &self.discovery_ctk {
            discovert_ctk.cancel();
            self.discovery_ctk = None;
        }
    }

    pub fn change_visibility(&mut self, nv: Visibility) {
        self.visibility_sender
            .lock()
            .unwrap()
            .send_modify(|state| *state = nv);
    }

    pub async fn stop(&mut self) {
        self.stop_discovery();

        if let Some(ctoken) = &self.ctoken {
            ctoken.cancel();
        }

        if let Some(tracker) = &self.tracker {
            // Inorder for TaskTracker::wait to return, close() must be called
            // and the count of tasks being watched should be 0 (i.e. they've all closed).
            //
            // If not, the TaskTracker may forever wait if task count is 0 when wait() was called
            tracker.close();
            tracker.wait().await;
        }

        self.ctoken = None;
        self.tracker = None;
    }

    // Setting None here will resume the default settings
    pub fn set_download_path(&self, p: Option<PathBuf>) {
        debug!("Setting the download path to {:?}", p);
        let mut guard = CUSTOM_DOWNLOAD.write().unwrap();
        *guard = p;
    }

    /// For this to properly take effect,
    /// `MdnsServer` would need to be reset which is done by `RQS::stop` followed by `RQS::run`.
    ///
    /// So only do this when no data transfer is going on.
    pub fn set_device_name(&self, name: String) {
        debug!("Setting the device name {:?}", name);
        let mut guard = DEVICE_NAME.write().unwrap();
        *guard = name;
    }

    /// Enables the WIFI_HOTSPOT bandwidth upgrade through `link`; `None` disables it.
    /// Takes effect for sessions that start afterwards.
    pub fn set_wifi_link(&self, link: Option<Arc<dyn WifiLink>>) {
        debug!(
            "Wi-Fi upgrade {}",
            if link.is_some() { "on" } else { "off" }
        );
        // A poisoned lock still holds a valid Option, so recover it.
        let present = link.is_some();
        *WIFI_LINK.write().unwrap_or_else(|e| e.into_inner()) = link;
        WIFI_LINK_SET.send_replace(present);
    }

    pub fn get_device_name(&self) -> String {
        DEVICE_NAME.read().unwrap().clone()
    }
}

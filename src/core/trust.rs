//! Trusted devices: pairing, and authenticated announcements that let a paired device's transfer be accepted without asking.
//!
//! Peers talk over a Noise XX channel. Pairing shows an 8-digit code derived from the handshake on both screens; if the user confirms they match, each side stores the other's public key. Later the sender announces a transfer over that channel and the receiver accepts the Quick Share transfer arriving from the same IP shortly after.

use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    net::{IpAddr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::{Builder, TransportState, params::NoiseParams};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::timeout,
};

use crate::{history, ratelimit::RateLimit};

pub const DEFAULT_PORT: u16 = 47891;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
const INTENT_TTL: Duration = Duration::from_secs(60);
const PAIRING_WINDOW: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: usize = 30;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const MAX_OPEN: usize = 32;
const MAX_OPEN_PER_ADDRESS: usize = 4;
const HELLO_PROBE: u8 = 1;
const HELLO_INTENT: u8 = 2;
const HELLO_PAIR: u8 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub fingerprint: String,
    pub public_key: String,
    pub paired_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    private_key: String,
    public_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrustEvent {
    PairingCode {
        code: String,
        peer: String,
    },
    PairingFinished {
        ok: bool,
        peer: String,
        message: String,
    },
    PeersChanged,
}

#[derive(Debug, PartialEq)]
pub struct ProbeResult {
    pub name: String,
    pub trusted: bool,
    /// The other computer has its pairing window open (or is too old to say).
    pub pairing: bool,
}

pub struct Trust {
    identity: Identity,
    peers_path: Option<PathBuf>,
    peers: Mutex<Vec<Peer>>,
    name: Mutex<String>,
    intents: Mutex<HashMap<IpAddr, (String, Instant)>>,
    pairing_until: Mutex<Option<Instant>>,
    pending: Mutex<Option<oneshot::Sender<bool>>>,
    events: mpsc::UnboundedSender<TrustEvent>,
    limit: Mutex<RateLimit>,
    slots: Arc<Semaphore>,
    open: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

struct Admission {
    _slot: OwnedSemaphorePermit,
    ip: IpAddr,
    open: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap();
        if let Some(n) = open.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.ip);
            }
        }
    }
}

pub fn default_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("kuickshare"))
}

fn params() -> Result<NoiseParams> {
    Ok(PATTERN.parse()?)
}

fn fingerprint(public_key: &[u8]) -> String {
    hex::encode(Sha256::digest(public_key))
}

/// 8 digits from the handshake hash, shown as "1234 5678". A man in the middle can grind keys until
/// both sides show the same code: feasible in under a minute with 6 digits, not within the confirm time with 8.
fn short_code(handshake_hash: &[u8]) -> String {
    let bytes: [u8; 4] = handshake_hash[..4].try_into().unwrap_or_default();
    let number = u32::from_be_bytes(bytes) % 100_000_000;
    format!("{:04} {:04}", number / 10_000, number % 10_000)
}

fn load_or_create_identity(path: Option<&Path>) -> Result<Identity> {
    if let Some(path) = path {
        // Only a missing file means "first run": any other error must not replace the key.
        match fs::read_to_string(path) {
            Ok(text) => {
                return serde_json::from_str(&text)
                    .with_context(|| format!("invalid identity file {}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", path.display()));
            }
        }
    }
    let keypair = Builder::new(params()?).generate_keypair()?;
    let identity = Identity {
        private_key: hex::encode(keypair.private),
        public_key: hex::encode(keypair.public),
    };
    if let Some(path) = path {
        fs::create_dir_all(path.parent().context("identity path has no parent")?)?;
        // Private key: mode 0600, and a crash must not leave a half-written file (the daemon would
        // refuse to start), so write a temporary file and rename it.
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(serde_json::to_string(&identity)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("cannot create {}", path.display()))?;
    }
    Ok(identity)
}

async fn write_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).context("frame too large")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(data).await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let mut data = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut data).await?;
    Ok(data)
}

struct Session {
    stream: TcpStream,
    transport: TransportState,
    remote_key: Vec<u8>,
    remote_name: String,
    hash: Vec<u8>,
}

impl Session {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; data.len() + 64];
        let n = self.transport.write_message(data, &mut buf)?;
        write_frame(&mut self.stream, &buf[..n]).await
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        let frame = read_frame(&mut self.stream).await?;
        let mut buf = vec![0u8; frame.len()];
        let n = self.transport.read_message(&frame, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn recv_within(&mut self, limit: Duration) -> Result<Vec<u8>> {
        timeout(limit, self.recv())
            .await
            .map_err(|_| anyhow!("timed out"))?
    }
}

impl Trust {
    /// `dir` holds `identity.json` (created on first use, private) and `peers.json`.
    pub fn load(
        dir: Option<&Path>,
        name: String,
        events: mpsc::UnboundedSender<TrustEvent>,
    ) -> Result<Arc<Self>> {
        let identity = load_or_create_identity(dir.map(|d| d.join("identity.json")).as_deref())?;
        let peers_path = dir.map(|d| d.join("peers.json"));
        let peers = peers_path
            .as_deref()
            .and_then(|p| {
                let text = fs::read_to_string(p).ok()?;
                match serde_json::from_str(&text) {
                    Ok(peers) => Some(peers),
                    Err(e) => {
                        tracing::warn!("ignoring invalid peers file {}: {e}", p.display());
                        crate::settings::keep_aside(p);
                        None
                    }
                }
            })
            .unwrap_or_default();
        Ok(Arc::new(Self {
            identity,
            peers_path,
            peers: Mutex::new(peers),
            name: Mutex::new(name),
            intents: Mutex::default(),
            pairing_until: Mutex::default(),
            pending: Mutex::default(),
            events,
            limit: Mutex::new(RateLimit::new(MAX_ATTEMPTS, ATTEMPT_WINDOW)),
            slots: Arc::new(Semaphore::new(MAX_OPEN)),
            open: Arc::default(),
        }))
    }

    /// Refuses an address that connects too often or holds too many connections, or when the total
    /// is reached. The admission is held while the connection is served.
    fn admit(&self, ip: IpAddr, now: Instant) -> Option<Admission> {
        if self.open.lock().unwrap().get(&ip).copied().unwrap_or(0) >= MAX_OPEN_PER_ADDRESS
            || !self.limit.lock().unwrap().allow(ip, now)
        {
            return None;
        }
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        *self.open.lock().unwrap().entry(ip).or_default() += 1;
        Some(Admission {
            _slot: slot,
            ip,
            open: self.open.clone(),
        })
    }

    pub fn set_name(&self, name: String) {
        *self.name.lock().unwrap() = name;
    }

    pub fn peers_json(&self) -> String {
        serde_json::to_string(&*self.peers.lock().unwrap()).unwrap_or_else(|_| "[]".to_owned())
    }

    pub fn unpair(&self, fingerprint: &str) {
        self.peers
            .lock()
            .unwrap()
            .retain(|p| p.fingerprint != fingerprint);
        self.save_peers();
        let _ = self.events.send(TrustEvent::PeersChanged);
    }

    fn peer_with_key(&self, public_key: &[u8]) -> Option<Peer> {
        let key = hex::encode(public_key);
        self.peers
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.public_key == key)
            .cloned()
    }

    fn add_peer(&self, name: &str, public_key: &[u8]) -> Peer {
        let peer = Peer {
            name: name.to_owned(),
            fingerprint: fingerprint(public_key),
            public_key: hex::encode(public_key),
            paired_at: history::now(),
        };
        {
            let mut peers = self.peers.lock().unwrap();
            peers.retain(|p| p.public_key != peer.public_key);
            peers.push(peer.clone());
        }
        self.save_peers();
        let _ = self.events.send(TrustEvent::PeersChanged);
        peer
    }

    fn save_peers(&self) {
        let Some(path) = &self.peers_path else {
            return;
        };
        let json = self.peers_json();
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, path)) {
            tracing::warn!("cannot save trusted devices: {e}");
        }
    }

    /// While open, other computers may start a pairing with this one.
    pub fn set_pairing(&self, open: bool) {
        *self.pairing_until.lock().unwrap() = open.then(|| Instant::now() + PAIRING_WINDOW);
    }

    pub fn pairing_open(&self) -> bool {
        self.pairing_until
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until)
    }

    pub fn confirm_pairing(&self, accept: bool) {
        if let Some(reply) = self.pending.lock().unwrap().take() {
            let _ = reply.send(accept);
        }
    }

    /// Consumes a fresh announcement from this IP, returning the name of the paired device.
    pub fn take_intent(&self, ip: &str) -> Option<String> {
        let ip: IpAddr = ip.parse().ok()?;
        let (name, at) = self.intents.lock().unwrap().remove(&ip)?;
        (at.elapsed() < INTENT_TTL).then_some(name)
    }

    pub async fn listen(self: &Arc<Self>, addr: SocketAddr) -> Result<SocketAddr> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        let bound = listener.local_addr()?;
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, remote)) = listener.accept().await else {
                    // Out of file descriptors, for instance: do not spin on the error.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let Some(permit) = this.admit(remote.ip(), Instant::now()) else {
                    tracing::debug!("trust connection from {remote} refused: too many");
                    continue;
                };
                let this = this.clone();
                tokio::spawn(async move {
                    let _admission = permit;
                    if let Err(e) = this.handle(stream, remote.ip()).await {
                        tracing::debug!("trust connection from {remote} ended: {e:#}");
                    }
                });
            }
        });
        Ok(bound)
    }

    async fn handshake(&self, mut stream: TcpStream, initiator: bool) -> Result<Session> {
        let name = self.name.lock().unwrap().clone();
        let private_key = hex::decode(&self.identity.private_key)?;
        let builder = Builder::new(params()?).local_private_key(&private_key)?;
        let mut state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        let mut buf = vec![0u8; 1024];
        let mut remote_name = String::new();
        let mut step = 0;
        while !state.is_handshake_finished() {
            // XX: -> e, <- e ee s es (+name), -> s se (+name). Even steps are ours when initiating.
            let ours = (step % 2 == 0) == initiator;
            if ours {
                let payload = if step == 0 {
                    Vec::new()
                } else {
                    name.clone().into_bytes()
                };
                let n = state.write_message(&payload, &mut buf)?;
                timeout(IO_TIMEOUT, write_frame(&mut stream, &buf[..n]))
                    .await
                    .map_err(|_| anyhow!("timed out"))??;
            } else {
                let frame = timeout(IO_TIMEOUT, read_frame(&mut stream))
                    .await
                    .map_err(|_| anyhow!("timed out"))??;
                let n = state.read_message(&frame, &mut buf)?;
                if step > 0 {
                    remote_name = String::from_utf8_lossy(&buf[..n]).into_owned();
                }
            }
            step += 1;
        }
        let remote_key = state
            .get_remote_static()
            .context("peer sent no key")?
            .to_vec();
        let hash = state.get_handshake_hash().to_vec();
        Ok(Session {
            stream,
            transport: state.into_transport_mode()?,
            remote_key,
            remote_name,
            hash,
        })
    }

    async fn handle(self: Arc<Self>, stream: TcpStream, remote: IpAddr) -> Result<()> {
        let mut session = self.handshake(stream, false).await?;
        let hello = session.recv_within(IO_TIMEOUT).await?;
        let known = self.peer_with_key(&session.remote_key);
        match hello.first().copied() {
            Some(HELLO_PROBE) => {
                session
                    .send(&[1, u8::from(known.is_some()), u8::from(self.pairing_open())])
                    .await
            }
            Some(HELLO_INTENT) => match known {
                Some(peer) => {
                    self.intents
                        .lock()
                        .unwrap()
                        .insert(remote, (peer.name, Instant::now()));
                    session.send(&[1]).await
                }
                None => session.send(&[0]).await,
            },
            // A computer with no name could not be told from another in the lists.
            Some(HELLO_PAIR) if self.pairing_open() && !session.remote_name.trim().is_empty() => {
                session.send(&[1]).await?;
                self.pairing_flow(&mut session).await.map(|_| ())
            }
            _ => session.send(&[0]).await,
        }
    }

    async fn connect(&self, addr: SocketAddr) -> Result<Session> {
        let stream = timeout(Duration::from_secs(3), TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("connection timed out"))??;
        self.handshake(stream, true).await
    }

    /// Asks a computer whether it runs KuickShare, and whether it trusts this one.
    pub async fn probe(&self, addr: SocketAddr) -> Result<ProbeResult> {
        let mut session = self.connect(addr).await?;
        session.send(&[HELLO_PROBE]).await?;
        let reply = session.recv_within(IO_TIMEOUT).await?;
        let they_trust = reply.get(1) == Some(&1);
        // Computers from before the flag existed send two bytes: assume the window is open and let pairing say.
        let pairing = reply.get(2) != Some(&0);
        let we_trust = self.peer_with_key(&session.remote_key).is_some();
        Ok(ProbeResult {
            name: session.remote_name,
            trusted: they_trust && we_trust,
            pairing,
        })
    }

    /// Tells a paired computer that a transfer is about to arrive from this one.
    pub async fn announce(&self, addr: SocketAddr) -> Result<()> {
        let mut session = self.connect(addr).await?;
        if self.peer_with_key(&session.remote_key).is_none() {
            bail!("the other computer is not paired");
        }
        session.send(&[HELLO_INTENT]).await?;
        match session.recv_within(IO_TIMEOUT).await?.first() {
            Some(1) => Ok(()),
            _ => bail!("the other computer does not trust this one"),
        }
    }

    pub async fn pair(&self, addr: SocketAddr) -> Result<Peer> {
        let mut session = self.connect(addr).await?;
        session.send(&[HELLO_PAIR]).await?;
        if session.recv_within(IO_TIMEOUT).await?.first() != Some(&1) {
            let message = "The other computer is not accepting pairing requests".to_owned();
            self.finish(false, &session.remote_name, &message);
            bail!(message);
        }
        self.pairing_flow(&mut session).await
    }

    async fn pairing_flow(&self, session: &mut Session) -> Result<Peer> {
        if session.remote_name.trim().is_empty() {
            self.finish(false, "", "The other computer has no name");
            bail!("The other computer has no name");
        }
        // One pairing at a time: another host must not be able to take over the code on screen.
        let (reply, answer) = oneshot::channel();
        {
            let mut slot = self.pending.lock().unwrap();
            if slot.is_some() {
                bail!("Another pairing is in progress");
            }
            *slot = Some(reply);
        }
        let result = self.confirm_codes(session, answer).await;
        // Only the flow that owns the slot clears it (`finish` is also called without owning it).
        self.pending.lock().unwrap().take();
        match &result {
            Ok(peer) => self.finish(true, &peer.name, ""),
            Err(e) => self.finish(false, &session.remote_name, &format!("{e:#}")),
        }
        result
    }

    async fn confirm_codes(
        &self,
        session: &mut Session,
        answer: oneshot::Receiver<bool>,
    ) -> Result<Peer> {
        let _ = self.events.send(TrustEvent::PairingCode {
            code: short_code(&session.hash),
            peer: session.remote_name.clone(),
        });
        let mine = timeout(PAIRING_TIMEOUT, answer)
            .await
            .map_err(|_| anyhow!("Nobody confirmed the code in time"))?
            .unwrap_or(false);
        session.send(&[u8::from(mine)]).await?;
        let theirs = session.recv_within(PAIRING_TIMEOUT).await?;
        if !mine {
            bail!("Pairing was cancelled");
        }
        if theirs.first() != Some(&1) {
            bail!("The other computer did not confirm the code");
        }
        Ok(self.add_peer(&session.remote_name, &session.remote_key))
    }

    fn finish(&self, ok: bool, peer: &str, message: &str) {
        let _ = self.events.send(TrustEvent::PairingFinished {
            ok,
            peer: peer.to_owned(),
            message: message.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Node {
        trust: Arc<Trust>,
        events: mpsc::UnboundedReceiver<TrustEvent>,
        addr: SocketAddr,
    }

    async fn node(name: &str) -> Node {
        let (tx, events) = mpsc::unbounded_channel();
        let trust = Trust::load(None, name.to_owned(), tx).unwrap();
        let addr = trust.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        Node {
            trust,
            events,
            addr,
        }
    }

    async fn next_code(node: &mut Node) -> String {
        loop {
            if let TrustEvent::PairingCode { code, .. } = node.events.recv().await.unwrap() {
                return code;
            }
        }
    }

    /// Pairs `a` (initiator) with `b`; each side answers its code prompt as given.
    async fn pair(a: &mut Node, b: &mut Node, a_accepts: bool, b_accepts: bool) -> Result<Peer> {
        b.trust.set_pairing(true);
        let initiator = a.trust.clone();
        let addr = b.addr;
        let task = tokio::spawn(async move { initiator.pair(addr).await });

        let (code_a, code_b) = (next_code(a).await, next_code(b).await);
        assert_eq!(code_a, code_b, "both screens must show the same code");
        assert_eq!(code_a.len(), 9); // "1234 5678"
        a.trust.confirm_pairing(a_accepts);
        b.trust.confirm_pairing(b_accepts);
        task.await.unwrap()
    }

    #[tokio::test]
    async fn paired_devices_trust_each_other() {
        let (mut a, mut b) = (node("alice").await, node("bob").await);
        let peer = pair(&mut a, &mut b, true, true).await.unwrap();
        assert_eq!(peer.name, "bob");

        let alice_peers = a.trust.peers_json();
        assert_eq!(alice_peers.matches("bob").count(), 1);
        assert!(b.trust.peers_json().contains("alice"));

        let probe = a.trust.probe(b.addr).await.unwrap();
        assert_eq!(
            probe,
            ProbeResult {
                name: "bob".into(),
                trusted: true,
                pairing: true
            }
        );
        a.trust.announce(b.addr).await.unwrap();
        assert_eq!(b.trust.take_intent("127.0.0.1").as_deref(), Some("alice"));
        assert_eq!(
            b.trust.take_intent("127.0.0.1"),
            None,
            "an intent is used once"
        );
    }

    #[tokio::test]
    async fn refused_pair_with_keeps_the_waiting_incoming_pairing() {
        let (a, b) = (node("alice").await, node("bob").await);
        // b does not accept pairing, so a's `pair` is refused before owning the slot.
        let (reply, _answer) = oneshot::channel();
        *a.trust.pending.lock().unwrap() = Some(reply);
        assert!(a.trust.pair(b.addr).await.is_err());
        assert!(a.trust.pending.lock().unwrap().is_some());
    }

    #[test]
    fn identity_read_error_does_not_regenerate_the_key() {
        let dir = std::env::temp_dir().join(format!("kuick-a4-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A directory at the identity path: reading fails with an error other than NotFound.
        let path = dir.join("identity.json");
        fs::create_dir_all(&path).unwrap();
        assert!(load_or_create_identity(Some(&path)).is_err());
        assert!(path.is_dir());
        let missing = dir.join("new/identity.json");
        assert!(load_or_create_identity(Some(&missing)).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn declined_pairing_stores_nothing() {
        let (mut a, mut b) = (node("alice").await, node("bob").await);
        assert!(pair(&mut a, &mut b, true, false).await.is_err());
        assert_eq!(a.trust.peers_json(), "[]");
        assert_eq!(b.trust.peers_json(), "[]");
    }

    #[tokio::test]
    async fn strangers_cannot_announce_and_pairing_must_be_open() {
        let (a, b) = (node("alice").await, node("bob").await);
        assert!(a.trust.announce(b.addr).await.is_err());
        assert_eq!(b.trust.take_intent("127.0.0.1"), None);
        let probe = a.trust.probe(b.addr).await.unwrap();
        assert!(!probe.trusted);
        assert!(!probe.pairing);
        // Pairing window closed.
        assert!(a.trust.pair(b.addr).await.is_err());
    }

    #[tokio::test]
    async fn a_computer_without_a_name_cannot_pair() {
        let (a, b) = (node("").await, node("bob").await);
        b.trust.set_pairing(true);
        assert!(a.trust.pair(b.addr).await.is_err());
        assert_eq!(b.trust.peers_json(), "[]");
    }

    #[tokio::test]
    async fn unpair_forgets_the_device() {
        let (mut a, mut b) = (node("alice").await, node("bob").await);
        let peer = pair(&mut a, &mut b, true, true).await.unwrap();
        a.trust.unpair(&peer.fingerprint);
        assert_eq!(a.trust.peers_json(), "[]");
        assert!(!a.trust.probe(b.addr).await.unwrap().trusted);
    }

    /// REPRO (defect 8): an intent announced by a paired computer must not
    /// outlive the pairing.
    #[tokio::test]
    async fn unpaired_peer_intent_is_void() {
        let (mut a, mut b) = (node("alice").await, node("bob").await);
        pair(&mut a, &mut b, true, true).await.unwrap();
        a.trust.announce(b.addr).await.unwrap();
        // The intent is stored on b before it answers, so it is there now.
        let fingerprint = b.trust.peers.lock().unwrap()[0].fingerprint.clone();
        b.trust.unpair(&fingerprint);
        assert_eq!(b.trust.peers_json(), "[]");
        assert_eq!(
            b.trust.take_intent("127.0.0.1"),
            None,
            "an unpaired computer's intent must be void"
        );
    }

    #[tokio::test]
    async fn an_address_that_connects_too_often_is_refused() {
        let node = node("limited").await;
        let busy = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();
        // Each connection is over before the next one: only the rate limit applies.
        for _ in 0..MAX_ATTEMPTS {
            assert!(node.trust.admit(busy, now).is_some(), "under the limit");
        }
        assert!(node.trust.admit(busy, now).is_none());
        assert!(node.trust.admit(IpAddr::from([10, 0, 0, 2]), now).is_some());
    }

    #[tokio::test]
    async fn one_address_cannot_hold_many_connections_open() {
        let node = node("per-address").await;
        let busy = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();
        let held: Vec<_> = (0..MAX_OPEN_PER_ADDRESS)
            .map(|_| {
                node.trust
                    .admit(busy, now)
                    .expect("a place for this address")
            })
            .collect();
        assert!(node.trust.admit(busy, now).is_none());
        assert!(node.trust.admit(IpAddr::from([10, 0, 0, 2]), now).is_some());
        drop(held);
        assert!(node.trust.admit(busy, now).is_some());
    }

    #[tokio::test]
    async fn open_connections_are_capped_for_all_addresses_together() {
        let node = node("capped").await;
        let now = Instant::now();
        let held: Vec<_> = (0..MAX_OPEN)
            .map(|n| {
                let ip = IpAddr::from([10, 1, 0, n as u8]);
                node.trust.admit(ip, now).expect("a free slot")
            })
            .collect();
        assert!(node.trust.admit(IpAddr::from([10, 2, 0, 1]), now).is_none());
        drop(held);
        assert!(node.trust.admit(IpAddr::from([10, 2, 0, 2]), now).is_some());
    }

    #[test]
    fn code_has_eight_digits_in_two_groups() {
        assert_eq!(short_code(&[0, 0, 0, 7, 9]), "0000 0007");
        assert_eq!(short_code(&[0xff, 0xff, 0xff, 0xff]), "9496 7295");
    }
}

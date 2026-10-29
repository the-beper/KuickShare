//! Watches the system bus for what changes a report, and drops the engine's cache.
//!
//! One system bus connection of its own, with match rules that name the sender, the interface and
//! (where it cuts the traffic) `arg0`: the owner of BlueZ, NetworkManager or firewalld changing;
//! BlueZ adapters and their managers appearing or going (the ObjectManager signals are all matched
//! and filtered in process, since a match rule cannot name interfaces inside their arguments;
//! device objects are dropped there); an adapter's `Powered`, `PowerState` or
//! `Roles` changing; NetworkManager's devices, its radio switches, and a device's `Managed`,
//! `State`, `FirmwareMissing` or `NmPluginMissing` changing. Advertisements and scan results are
//! not matched at all. Each event only calls `invalidate`; the engine debounces and
//! limits the runs. If the bus cannot be reached the task ends quietly: there is no polling.

use std::{collections::HashMap, num::NonZeroUsize, pin::Pin, sync::Arc};

use futures_util::{Stream, StreamExt as _, stream::select_all};
use zbus::{
    Connection, MatchRule, Message, MessageStream, OwnedObjectPath, OwnedVariant, message::Type,
};

use super::engine::{Invalidation, RequirementsEngine, RequirementsProbe};

const BLUEZ: &str = "org.bluez";
const NM: &str = "org.freedesktop.NetworkManager";
const FIREWALLD: &str = "org.fedoraproject.FirewallD1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Which rule a message came through; it decides what in the message matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Owner,
    BluezObjects,
    BluezAdapter,
    NmDevices,
    NmManagerProps,
    NmDeviceProps,
}

fn signal() -> zbus::match_rule::Builder<'static> {
    MatchRule::builder().msg_type(Type::Signal)
}

fn rules() -> zbus::Result<Vec<(MatchRule<'static>, Source)>> {
    let mut rules = Vec::new();
    for name in [BLUEZ, NM, FIREWALLD] {
        rules.push((
            signal()
                .sender("org.freedesktop.DBus")
                .interface("org.freedesktop.DBus")
                .member("NameOwnerChanged")
                .arg(0, name)
                .build()?,
            Source::Owner,
        ));
    }
    rules.push((
        signal()
            .sender(BLUEZ)
            .interface("org.freedesktop.DBus.ObjectManager")
            .build()?,
        Source::BluezObjects,
    ));
    rules.push((
        signal()
            .sender(BLUEZ)
            .interface(PROPERTIES)
            .member("PropertiesChanged")
            .path_namespace("/org/bluez")
            .arg(0, "org.bluez.Adapter1")
            .build()?,
        Source::BluezAdapter,
    ));
    for member in ["DeviceAdded", "DeviceRemoved"] {
        rules.push((
            signal().sender(NM).interface(NM).member(member).build()?,
            Source::NmDevices,
        ));
    }
    rules.push((
        signal()
            .sender(NM)
            .interface(PROPERTIES)
            .member("PropertiesChanged")
            .path("/org/freedesktop/NetworkManager")
            .arg(0, NM)
            .build()?,
        Source::NmManagerProps,
    ));
    rules.push((
        signal()
            .sender(NM)
            .interface(PROPERTIES)
            .member("PropertiesChanged")
            .path_namespace("/org/freedesktop/NetworkManager/Devices")
            .arg(0, "org.freedesktop.NetworkManager.Device")
            .build()?,
        Source::NmDeviceProps,
    ));
    Ok(rules)
}

/// Whether a `PropertiesChanged` message names one of `keys` (changed or invalidated). A message
/// that cannot be read counts: a missed change is worse than one extra run.
fn properties_touch(msg: &Message, keys: &[&str]) -> bool {
    let Ok((_, changed, invalidated)) =
        msg.body()
            .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
    else {
        return true;
    };
    changed
        .keys()
        .chain(invalidated.iter())
        .any(|k| keys.contains(&k.as_str()))
}

/// Interfaces of the objects BlueZ adds or removes that matter here. Device objects, which come
/// and go with every scan, do not.
const BLUEZ_INTERFACES: [&str; 3] = [
    "org.bluez.Adapter1",
    "org.bluez.LEAdvertisingManager1",
    "org.bluez.GattManager1",
];

fn objects_touch_adapter(msg: &Message) -> bool {
    let added = msg
        .header()
        .member()
        .is_some_and(|m| m.as_str() == "InterfacesAdded");
    let names: Vec<String> = if added {
        match msg.body().deserialize::<(
            OwnedObjectPath,
            HashMap<String, HashMap<String, OwnedVariant>>,
        )>() {
            Ok((_, interfaces)) => interfaces.into_keys().collect(),
            Err(_) => return true,
        }
    } else {
        match msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>() {
            Ok((_, interfaces)) => interfaces,
            Err(_) => return true,
        }
    };
    names.iter().any(|n| BLUEZ_INTERFACES.contains(&n.as_str()))
}

fn relevance(source: Source, msg: &Message) -> Option<Invalidation> {
    match source {
        Source::Owner => Some(Invalidation::ServiceOwner),
        Source::NmDevices => Some(Invalidation::NetworkManager),
        Source::BluezObjects => objects_touch_adapter(msg).then_some(Invalidation::Bluetooth),
        Source::BluezAdapter => properties_touch(msg, &["Powered", "PowerState", "Roles"])
            .then_some(Invalidation::Bluetooth),
        Source::NmManagerProps => properties_touch(
            msg,
            &[
                "NetworkingEnabled",
                "WirelessEnabled",
                "WirelessHardwareEnabled",
                "Devices",
            ],
        )
        .then_some(Invalidation::NetworkManager),
        Source::NmDeviceProps => properties_touch(
            msg,
            &["Managed", "State", "FirmwareMissing", "NmPluginMissing"],
        )
        .then_some(Invalidation::NetworkManager),
    }
}

type Tagged = Pin<Box<dyn Stream<Item = (Source, zbus::Result<Message>)> + Send>>;

async fn watch<P: RequirementsProbe>(engine: Arc<RequirementsEngine<P>>) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let mut streams: Vec<Tagged> = Vec::new();
    for (rule, source) in rules()? {
        match MessageStream::for_match_rule(rule, &conn, NonZeroUsize::new(32)).await {
            Ok(stream) => streams.push(Box::pin(stream.map(move |m| (source, m)))),
            // Expected in a sandbox that hides the name.
            Err(e) => tracing::debug!("not watching {source:?}: {e}"),
        }
    }
    if streams.is_empty() {
        return Ok(());
    }
    let mut all = select_all(streams);
    while let Some((source, message)) = all.next().await {
        let Ok(message) = message else {
            continue;
        };
        if let Some(why) = relevance(source, &message) {
            engine.invalidate(why);
        }
    }
    Ok(())
}

/// Starts watching; the task ends when the bus goes away.
pub fn spawn<P: RequirementsProbe>(
    engine: Arc<RequirementsEngine<P>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = watch(engine).await {
            tracing::debug!("not watching the system services: {e}");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_match_rule_is_valid() {
        let rules = rules().unwrap();
        assert_eq!(rules.len(), 9);
        let owner = rules[0].0.to_string();
        assert!(owner.contains("NameOwnerChanged") && owner.contains("arg0='org.bluez'"));
    }
}

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use tokio::sync::mpsc;

const SERVICE_TYPE: &str = "_synchronize._tcp.local.";

#[derive(Debug, Clone)]
pub struct Peer {
    pub host: String,
    pub port: u16,
}

#[derive(Debug)]
pub enum DiscoveryEvent {
    Found(Peer),
}

pub fn start_discovery(
    service_name: &str,
    port: u16,
) -> Result<(ServiceDaemon, mpsc::Receiver<DiscoveryEvent>)> {
    let mdns = ServiceDaemon::new()?;

    let host = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        service_name,
        &format!("{host}.local."),
        "",
        port,
        HashMap::<String, String>::new(),
    )?;
    mdns.register(info)?;

    let receiver = mdns.browse(SERVICE_TYPE)?;
    let (tx, rx) = mpsc::channel::<DiscoveryEvent>(16);
    let own = service_name.to_string();

    tokio::spawn(async move {
        while let Ok(event) = receiver.recv_async().await {
            if let ServiceEvent::ServiceResolved(info) = event {
                let name = info.get_fullname().to_string();
                // 过滤自身服务，防止自己连自己导致死锁
                if name.contains(&own) {
                    continue;
                }
                if let Some(addr) = info.get_addresses().iter().next() {
                    let _ = tx.send(DiscoveryEvent::Found(Peer {
                        host: addr.to_string(),
                        port: info.get_port(),
                    })).await;
                }
            }
        }
    });

    Ok((mdns, rx))
}

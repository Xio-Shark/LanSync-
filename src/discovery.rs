use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use tokio::sync::mpsc;

const SERVICE_TYPE: &str = "_lansync._tcp.local.";

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

    // macOS 的 hostname::get() 返回 "xxx.local"，必须剥离 ".local"
    // 否则 mDNS host_name 变成 "xxx.local.local." 导致注册静默失败
    let raw_host = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let host = raw_host.trim_end_matches(".local").trim_end_matches('.');

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        service_name,
        &format!("{host}.local."),
        "",
        port,
        HashMap::<String, String>::new(),
    )?;
    tracing::info!("mDNS 注册: type={SERVICE_TYPE} name={service_name} host={host}.local. port={port}");
    mdns.register(info)?;

    let receiver = mdns.browse(SERVICE_TYPE)?;
    let (tx, rx) = mpsc::channel::<DiscoveryEvent>(16);
    let own = service_name.to_string();

    tokio::spawn(async move {
        while let Ok(event) = receiver.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let name = info.get_fullname().to_string();
                    tracing::debug!("mDNS resolved: {name}");
                    // 过滤自身服务，防止自己连自己导致死锁
                    if name.contains(&own) {
                        tracing::debug!("跳过自身: {name}");
                        continue;
                    }
                    if let Some(addr) = info.get_addresses().iter().next() {
                        let _ = tx.send(DiscoveryEvent::Found(Peer {
                            host: addr.to_string(),
                            port: info.get_port(),
                        })).await;
                    }
                }
                ServiceEvent::ServiceFound(_, full) => {
                    tracing::debug!("mDNS found: {full}");
                }
                ServiceEvent::SearchStarted(stype) => {
                    tracing::debug!("mDNS 搜索开始: {stype}");
                }
                _ => {}
            }
        }
    });

    Ok((mdns, rx))
}

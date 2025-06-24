use get_if_addrs::{get_if_addrs, IfAddr};
use std::net::IpAddr;
pub fn list_local_ip_addresses() -> Vec<IpAddr> {
    let mut ips = Vec::new();

    match get_if_addrs() {
        Ok(interfaces) => {
            for iface in interfaces {
                match iface.addr {
                    IfAddr::V4(addr) => ips.push(IpAddr::V4(addr.ip)),
                    IfAddr::V6(addr) => ips.push(IpAddr::V6(addr.ip)),
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to get interfaces: {}", e);
        }
    }

    ips
}
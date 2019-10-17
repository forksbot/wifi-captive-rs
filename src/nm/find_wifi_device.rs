//! This crate will immediately quit if no wifi device can be found. This module encapsulates the
//! method to find a wifi device via the network manager dbus API.

use super::{device, networkmanager, NM_BUSNAME, NM_PATH};
use crate::CaptivePortalError;
use dbus::nonblock;
use std::sync::Arc;

pub struct FindWifiDeviceResult {
    /// The network manager dbus api device path
    pub device_path: dbus::Path<'static>,
    /// The interface name
    pub interface_name: String,
    /// The mac address
    pub hw: String,
}

/// Finds the first wifi device or the wifi device on the given device interface.
/// Returns (wifi_device_path, interface_name) on success and an error otherwise.
pub async fn find_wifi_device(
    connection: Arc<dbus::nonblock::SyncConnection>,
    preferred_interface: &Option<String>,
) -> Result<FindWifiDeviceResult, CaptivePortalError> {
    let p = nonblock::Proxy::new(NM_BUSNAME, NM_PATH, connection.clone());

    // Get all devices (if possible: by interface)
    use networkmanager::NetworkManager;
    if let Some(interface_name) = preferred_interface {
        let device_path = p.get_device_by_ip_iface(&interface_name).await?;
        let device_data = nonblock::Proxy::new(NM_BUSNAME, &device_path, connection.clone());
        use device::Device;
        let device_type = device_data.device_type().await?;
        if device_type == super::connectivity::DeviceType::WiFi as u32 {
            use device::DeviceWireless;
            let hw = device_data.hw_address().await?;
            info!("Wireless device found: {}", interface_name);
            return Ok(FindWifiDeviceResult {
                device_path,
                interface_name: interface_name.clone(),
                hw,
            });
        }
    };

    // Filter by type; only wifi devices; take first
    let device_paths = p.get_all_devices().await?;
    for device_path in device_paths {
        let device_data = nonblock::Proxy::new(NM_BUSNAME, &device_path, connection.clone());
        use device::Device;
        let dtype = device_data.device_type().await?;
        if dtype == super::connectivity::DeviceType::WiFi as u32 {
            use device::DeviceWireless;
            let hw = device_data.hw_address().await?;
            let interface_name = device_data.interface().await?;
            info!("Wireless device on '{}'", &interface_name);
            return Ok(FindWifiDeviceResult {
                device_path,
                interface_name,
                hw,
            });
        }
    }

    Err(CaptivePortalError::no_wifi_device())
}

// Jackson Coxson

use idevice::{
    core_device_proxy::CoreDeviceProxy,
    provider::{IdeviceProvider, TcpProvider},
    usbmuxd::UsbmuxdConnection,
    IdeviceService, RsdService,
};
use log::{error, info};
use plist::Value;
use plist_plus::Plist;
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
};

use crate::{
    device::{fetch_first_device, test_device_connection},
    Errors, Res, RustyPlistConversion, RUNTIME,
};

#[swift_bridge::bridge]
mod ffi {
    #[swift_bridge(already_declared, swift_name = "MinimuxerError")]
    enum Errors {}

    extern "Rust" {
        fn install_provisioning_profile(profile: &[u8]) -> Result<(), Errors>;
        fn remove_provisioning_profile(id: String) -> Result<(), Errors>;
        fn dump_profiles(docs_path: String) -> Result<String, Errors>;
    }
}

// TODO: take a vec of provisioning profiles and remove old ones like AltServer
/// Installs a provisioning profile on the device
pub fn install_provisioning_profile(profile: &[u8]) -> Res<()> {
    info!("Installing provisioning profile");

    if !test_device_connection() {
        error!("No device connection");
        return Err(Errors::NoConnection);
    }

    let device = fetch_first_device()?;

    let ld_client = match device.new_lockdownd_client("minimuxer-prov-version") {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to connect to lockdown for version check: {e:?}");
            return Err(Errors::CreateLockdown);
        }
    };

    let product_version = match ld_client.get_value("ProductVersion", "") {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get ProductVersion: {e:?}");
            return Err(Errors::GetLockdownValue);
        }
    };

    let product_version = if let Some(v) = product_version
        .get_string_val()
        .ok()
        .and_then(|x| x.split('.').next().and_then(|s| s.parse::<u8>().ok()))
    {
        v
    } else {
        error!("Failed to parse ProductVersion as major version number");
        return Err(Errors::GetLockdownValue);
    };

    if product_version < 17 {
        let mis_client = match device.new_misagent_client("minimuxer-install-prov") {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to start misagent client: {:?}", e);
                return Err(Errors::CreateMisagent);
            }
        };

        let plist = Plist::new_data(profile);

        match mis_client.install(plist) {
            Ok(_) => {
                info!("Successfully installed provisioning profile!");
                Ok(())
            }
            Err(e) => {
                error!("Unable to install provisioning profile: {:?}", e);
                Err(Errors::ProfileInstall)
            }
        }
    } else {
        info!("iOS 17+ detected — using CoreDeviceProxy misagent path");
        let profile = profile.to_vec();
        RUNTIME.block_on(install_via_coredevice(profile))
    }
}

async fn install_via_coredevice(profile: Vec<u8>) -> Res<()> {
    let mut uc = UsbmuxdConnection::new(
        Box::new(
            match tokio::net::TcpStream::connect("127.0.0.1:27015").await {
                Ok(u) => u,
                Err(e) => {
                    error!("Failed to connect to usbmuxd: {e:?}");
                    return Err(Errors::NoConnection);
                }
            },
        ),
        0,
    );

    let dev = match uc
        .get_devices()
        .await
        .ok()
        .and_then(|x| x.into_iter().next())
    {
        Some(d) => d.to_provider(
            idevice::usbmuxd::UsbmuxdAddr::TcpSocket(std::net::SocketAddr::V4(
                SocketAddrV4::from_str("127.0.0.1:27015").unwrap(),
            )),
            "minimuxer",
        ),
        None => {
            error!("No device from usbmuxd");
            return Err(Errors::NoConnection);
        }
    };

    let provider = TcpProvider {
        addr: std::net::IpAddr::V4(Ipv4Addr::from_str("10.7.0.1").unwrap()),
        pairing_file: dev.get_pairing_file().await.unwrap(),
        label: "minimuxer".to_string(),
    };

    let proxy = match CoreDeviceProxy::connect(&provider).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to connect CoreDeviceProxy: {e:?}");
            return Err(Errors::CreateCoreDevice);
        }
    };

    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = match proxy.create_software_tunnel() {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to create software tunnel: {e:?}");
            return Err(Errors::CreateSoftwareTunnel);
        }
    };

    let mut handle = adapter.to_async_handle();
    let stream = match handle.connect(rsd_port).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to RSD port: {e:?}");
            return Err(Errors::Connect);
        }
    };

    let mut handshake = match idevice::rsd::RsdHandshake::new(stream).await {
        Ok(h) => h,
        Err(e) => {
            error!("Failed RSD handshake: {e:?}");
            return Err(Errors::XpcHandshake);
        }
    };

    let mut mis_client =
        match idevice::misagent::MisagentClient::connect_rsd(&mut handle, &mut handshake).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to connect MisagentClient via RSD: {e:?}");
                return Err(Errors::CreateMisagent);
            }
        };

    mis_client.install(profile).await.map_err(|e| {
        error!("Failed to install provisioning profile via RSD misagent: {e:?}");
        Errors::ProfileInstall
    })
}

/// Removes a provisioning profile
/// # Arguments
/// - `id`: Profile UUID
pub fn remove_provisioning_profile(id: String) -> Res<()> {
    info!("Removing profile with ID: {}", id);

    if !test_device_connection() {
        error!("No device connection");
        return Err(Errors::NoConnection);
    }

    let device = fetch_first_device()?;

    let ld_client = match device.new_lockdownd_client("minimuxer-prov-version") {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to connect to lockdown for version check: {e:?}");
            return Err(Errors::CreateLockdown);
        }
    };

    let product_version = match ld_client.get_value("ProductVersion", "") {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get ProductVersion: {e:?}");
            return Err(Errors::GetLockdownValue);
        }
    };

    let product_version = if let Some(v) = product_version
        .get_string_val()
        .ok()
        .and_then(|x| x.split('.').next().and_then(|s| s.parse::<u8>().ok()))
    {
        v
    } else {
        error!("Failed to parse ProductVersion as major version number");
        return Err(Errors::GetLockdownValue);
    };

    if product_version < 17 {
        let mis_client = match device.new_misagent_client("minimuxer-install-prov") {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to start misagent client: {:?}", e);
                return Err(Errors::CreateMisagent);
            }
        };

        match mis_client.remove(id) {
            Ok(_) => {
                info!("Successfully removed profile");
                Ok(())
            }
            Err(e) => {
                error!("Unable to remove provisioning profile: {:?}", e);
                Err(Errors::ProfileRemove)
            }
        }
    } else {
        info!("iOS 17+ detected — using CoreDeviceProxy misagent path for remove");
        RUNTIME.block_on(remove_via_coredevice(id))
    }
}

async fn remove_via_coredevice(id: String) -> Res<()> {
    let mut uc = UsbmuxdConnection::new(
        Box::new(
            match tokio::net::TcpStream::connect("127.0.0.1:27015").await {
                Ok(u) => u,
                Err(e) => {
                    error!("Failed to connect to usbmuxd: {e:?}");
                    return Err(Errors::NoConnection);
                }
            },
        ),
        0,
    );

    let dev = match uc
        .get_devices()
        .await
        .ok()
        .and_then(|x| x.into_iter().next())
    {
        Some(d) => d.to_provider(
            idevice::usbmuxd::UsbmuxdAddr::TcpSocket(std::net::SocketAddr::V4(
                SocketAddrV4::from_str("127.0.0.1:27015").unwrap(),
            )),
            "minimuxer",
        ),
        None => {
            error!("No device from usbmuxd");
            return Err(Errors::NoConnection);
        }
    };

    let provider = TcpProvider {
        addr: std::net::IpAddr::V4(Ipv4Addr::from_str("10.7.0.1").unwrap()),
        pairing_file: dev.get_pairing_file().await.unwrap(),
        label: "minimuxer".to_string(),
    };

    let proxy = match CoreDeviceProxy::connect(&provider).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to connect CoreDeviceProxy: {e:?}");
            return Err(Errors::CreateCoreDevice);
        }
    };

    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = match proxy.create_software_tunnel() {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to create software tunnel: {e:?}");
            return Err(Errors::CreateSoftwareTunnel);
        }
    };

    let mut handle = adapter.to_async_handle();
    let stream = match handle.connect(rsd_port).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to RSD port: {e:?}");
            return Err(Errors::Connect);
        }
    };

    let mut handshake = match idevice::rsd::RsdHandshake::new(stream).await {
        Ok(h) => h,
        Err(e) => {
            error!("Failed RSD handshake: {e:?}");
            return Err(Errors::XpcHandshake);
        }
    };

    let mut mis_client =
        match idevice::misagent::MisagentClient::connect_rsd(&mut handle, &mut handshake).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to connect MisagentClient via RSD: {e:?}");
                return Err(Errors::CreateMisagent);
            }
        };

    mis_client.remove(id).await.map_err(|e| {
        error!("Failed to remove provisioning profile via RSD misagent: {e:?}");
        Errors::ProfileRemove
    })
}

pub fn dump_profiles(docs_path: String) -> Res<String> {
    info!("Dumping profiles");

    if !test_device_connection() {
        error!("No device connection");
        return Err(Errors::NoConnection);
    }

    let device = fetch_first_device()?;

    let mis_client = match device.new_misagent_client("minimuxer-install-prov") {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to start misagent client: {:?}", e);
            return Err(Errors::CreateMisagent);
        }
    };

    let raw_profiles = match mis_client.copy(false) {
        Ok(m) => match Value::from_plist_plus(m) {
            Ok(v) => match v.as_array() {
                Some(a) => a.to_owned(),
                None => {
                    error!("Unable to convert to array");
                    return Err(Errors::ProfileRemove);
                }
            },
            Err(e) => {
                error!("Unable to convert to rusty plist: {:?}", e);
                return Err(Errors::ProfileRemove);
            }
        },
        Err(e) => {
            error!("Unable to copy profiles from misagent: {:?}", e);
            return Err(Errors::ProfileRemove);
        }
    };

    #[cfg(not(test))]
    let docs_path = docs_path[7..].to_string(); // remove the file:// prefix
    let dump_dir = format!(
        "{docs_path}/ProfileDump/{}",
        chrono::Local::now().format("%F_%I-%M-%S-%p")
    );
    std::fs::create_dir_all(&dump_dir).unwrap();

    for profile in raw_profiles {
        let data = match profile.as_data() {
            Some(c) => c.to_vec(),
            None => {
                error!("Unable to get profile as data");
                continue;
            }
        };

        const PLIST_PREFIX: &[u8] = b"<?xml version=";
        const PLIST_SUFFIX: &[u8] = b"</plist>";

        let prefix = match data
            .windows(PLIST_PREFIX.len())
            .position(|window| window == PLIST_PREFIX)
        {
            Some(p) => p,
            None => {
                error!("Unable to get prefix");
                continue;
            }
        };
        let suffix = match data
            .windows(PLIST_SUFFIX.len())
            .position(|window| window == PLIST_SUFFIX)
        {
            Some(p) => p,
            None => {
                error!("Unable to get suffix");
                continue;
            }
        }
            + PLIST_SUFFIX.len();

        let extracted_plist = &data[prefix..suffix];

        let plist = match Value::from_bytes(extracted_plist) {
            Ok(p) => match p.as_dictionary() {
                Some(d) => d.to_owned(),
                None => {
                    error!("Unable to convert plist to dictionary");
                    continue;
                }
            },
            Err(e) => {
                error!("Unable to convert cert bytes to plist: {:?}", e);
                continue;
            }
        };

        let uuid = match plist.get("UUID") {
            Some(e) => match e.as_string() {
                Some(d) => d.to_owned(),
                None => {
                    error!("Unable to convert UUID to string");
                    continue;
                }
            },
            None => {
                error!("Unable to get UUID");
                continue;
            }
        };

        std::fs::write(format!("{dump_dir}/{uuid}.mobileprovision",), &data).unwrap();
        std::fs::write(format!("{dump_dir}/{uuid}.plist",), extracted_plist).unwrap();
    }

    info!("Success");
    Ok(dump_dir)
}

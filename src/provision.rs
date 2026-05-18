// Jackson Coxson

use idevice::{
    misagent::MisagentClient,
    provider::{IdeviceProvider, TcpProvider},
    usbmuxd::UsbmuxdConnection,
    IdeviceService,
};
use log::{error, info};
use plist::Value;
use std::{net::SocketAddrV4, str::FromStr};

use crate::{
    device::{fetch_first_device, test_device_connection},
    muxer::DEVICE_IP,
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

    let profile = profile.to_vec();
    RUNTIME.block_on(install_via_tcp(profile))
}

async fn install_via_tcp(profile: Vec<u8>) -> Res<()> {
    let tcp_provider = make_tcp_provider().await?;

    let mut mis_client = match MisagentClient::connect(&tcp_provider).await {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to connect MisagentClient via TCP lockdownd: {e:?}");
            return Err(Errors::CreateMisagent);
        }
    };

    mis_client.install(profile).await.map_err(|e| {
        error!("Failed to install provisioning profile via TCP misagent: {e:?}");
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

    RUNTIME.block_on(remove_via_tcp(id))
}

async fn remove_via_tcp(id: String) -> Res<()> {
    let tcp_provider = make_tcp_provider().await?;

    let mut mis_client = match MisagentClient::connect(&tcp_provider).await {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to connect MisagentClient via TCP lockdownd: {e:?}");
            return Err(Errors::CreateMisagent);
        }
    };

    mis_client.remove(&id).await.map_err(|e| {
        error!("Failed to remove provisioning profile via TCP misagent: {e:?}");
        Errors::ProfileRemove
    })
}

/// Build a TcpProvider pointing at the device's known IP address.
/// Fetches the pairing file from the local usbmuxd proxy (which handles
/// ReadPairRecord without needing a Connect message).
async fn make_tcp_provider() -> Res<TcpProvider> {
    let mut uc = UsbmuxdConnection::new(
        Box::new(
            match tokio::net::TcpStream::connect("127.0.0.1:27015").await {
                Ok(u) => u,
                Err(e) => {
                    error!("Failed to connect to usbmuxd proxy: {e:?}");
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
            error!("No device returned from usbmuxd proxy");
            return Err(Errors::NoConnection);
        }
    };

    let pairing_file = match dev.get_pairing_file().await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get pairing file from usbmuxd proxy: {e:?}");
            return Err(Errors::PairingFile);
        }
    };

    let device_ip_str = DEVICE_IP.get().cloned().unwrap_or_else(|| "10.7.0.1".to_string());
    let device_ip = match std::net::IpAddr::from_str(&device_ip_str) {
        Ok(ip) => ip,
        Err(e) => {
            error!("Failed to parse device IP '{device_ip_str}': {e:?}");
            return Err(Errors::NoConnection);
        }
    };

    info!("TCP misagent: connecting to device at {device_ip}");
    Ok(TcpProvider {
        addr: device_ip,
        pairing_file,
        label: "minimuxer".to_string(),
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

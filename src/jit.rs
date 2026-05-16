// Jackson Coxson


use std::{
    net::SocketAddrV4,
    str::FromStr,
};

use idevice::{
    core_device_proxy::CoreDeviceProxy,
    debug_proxy::DebugProxyClient,
    usbmuxd::UsbmuxdConnection,
    IdeviceService, RsdService,
};
use log::{debug, error, info};
use plist_plus::Plist;
use rusty_libimobiledevice::services::instproxy::InstProxyClient;

use crate::{
    device::{fetch_first_device, test_device_connection},
    Errors, Res, RUNTIME,
};

#[swift_bridge::bridge]
mod ffi {
    #[swift_bridge(already_declared, swift_name = "MinimuxerError")]
    enum Errors {}

    extern "Rust" {
        fn debug_app(app_id: String) -> Result<(), Errors>;
        fn attach_debugger(pid: u32) -> Result<(), Errors>;
    }
}

/// Debugs an app from an app ID
pub fn debug_app(app_id: String) -> Res<()> {
    info!("Debugging app ID: {}", app_id);

    if !test_device_connection() {
        error!("No device connection");
        return Err(Errors::NoConnection);
    }

    let device = fetch_first_device()?;
    let ld_client = match device.new_lockdownd_client("minimuxer") {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to connect to lockdown: {e:?}");
            return Err(Errors::CreateLockdown);
        }
    };

    let product_version = match ld_client.get_value("ProductVersion", "") {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get product version from lockdown: {e:?}");
            return Err(Errors::GetLockdownValue);
        }
    };

    let product_version = if let Some(product_version) = product_version
        .get_string_val()
        .ok()
        .and_then(|x| x.split('.').collect::<Vec<&str>>()[0].parse::<u8>().ok())
    {
        product_version
    } else {
        error!("Failed to get product version from plist");
        return Err(Errors::GetLockdownValue);
    };

    if product_version < 17 {
        let debug_server = match device.new_debug_server("minimuxer") {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to start debug server: {:?}", e);
                return Err(Errors::CreateDebug);
            }
        };

        let instproxy_client = match device.new_instproxy_client("minimuxer") {
            Ok(i) => i,
            Err(e) => {
                error!("Failed to create instproxy client: {:?}", e);
                return Err(Errors::CreateInstproxy);
            }
        };

        let client_opts = InstProxyClient::create_return_attributes(
            vec![("ApplicationType".to_string(), Plist::new_string("Any"))],
            vec![
                "CFBundleIdentifier".to_string(),
                "CFBundleExecutable".to_string(),
                "CFBundlePath".to_string(),
                "BundlePath".to_string(),
                "Container".to_string(),
            ],
        );

        let lookup_results = match instproxy_client.lookup(vec![app_id.clone()], Some(client_opts))
        {
            Ok(apps) => {
                debug!("Successfully looked up apps: {:?}", apps);
                apps
            }
            Err(e) => {
                error!("Error looking up apps: {:?}", e);
                return Err(Errors::LookupApps);
            }
        };
        let lookup_results = lookup_results.dict_get_item(&app_id).unwrap();

        let working_directory = match lookup_results.dict_get_item("Container") {
            Ok(p) => p,
            Err(e) => {
                error!("App not found: {:?}", e);
                return Err(Errors::FindApp);
            }
        };

        let working_directory = match working_directory.get_string_val() {
            Ok(p) => p,
            Err(e) => {
                error!("Error when getting string val: {:?}", e);
                return Err(Errors::FindApp);
            }
        };
        debug!("Working directory: {}", working_directory);

        let bundle_path = match instproxy_client.get_path_for_bundle_identifier(app_id) {
            Ok(p) => p,
            Err(e) => {
                error!("Error getting path for bundle identifier: {:?}", e);
                return Err(Errors::BundlePath);
            }
        };
        info!("Successfully found bundle path: {bundle_path}");

        match debug_server.send_command("QSetMaxPacketSize: 1024".into()) {
            Ok(res) => info!("Successfully set max packet size: {:?}", res),
            Err(e) => {
                error!("Error setting max packet size: {:?}", e);
                return Err(Errors::MaxPacket);
            }
        }

        match debug_server.send_command(format!("QSetWorkingDir: {working_directory}").into()) {
            Ok(res) => info!("Successfully set working directory: {:?}", res),
            Err(e) => {
                error!("Error setting working directory: {:?}", e);
                return Err(Errors::WorkingDirectory);
            }
        }

        match debug_server.set_argv(vec![bundle_path.clone(), bundle_path]) {
            Ok(res) => info!("Successfully set argv: {:?}", res),
            Err(e) => {
                error!("Error setting argv: {:?}", e);
                return Err(Errors::Argv);
            }
        }

        match debug_server.send_command("qLaunchSuccess".into()) {
            Ok(res) => info!("Got launch response: {:?}", res),
            Err(e) => {
                error!("Error checking if app launched: {:?}", e);
                return Err(Errors::LaunchSuccess);
            }
        }

        match debug_server.send_command("D".into()) {
            Ok(res) => {
                info!("Success: {:?}", res);
                Ok(())
            }
            Err(e) => {
                error!("Error detaching: {:?}", e);
                Err(Errors::Detach)
            }
        }
    } else {
        RUNTIME.block_on(async move {
            let mut uc = UsbmuxdConnection::new(
                Box::new(
                    match tokio::net::TcpStream::connect("127.0.0.1:27015").await {
                        Ok(u) => u,
                        Err(_) => {
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
                    return Err(Errors::NoConnection);
                }
            };

            let proxy = match CoreDeviceProxy::connect(&dev).await {
                Ok(p) => p,
                Err(e) => {
                    error!("Failed to proxy device: {:?}", e);
                    return Err(Errors::CreateCoreDevice);
                }
            };

            let rsd_port = proxy.tunnel_info().server_rsd_port;
            let adapter = match proxy.create_software_tunnel() {
                Ok(a) => a,
                Err(e) => {
                    error!("Failed to create software tunnel: {:?}", e);
                    return Err(Errors::CreateSoftwareTunnel);
                }
            };

            let mut handle = adapter.to_async_handle();
            let stream = match handle.connect(rsd_port).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to RSD port: {:?}", e);
                    return Err(Errors::Connect);
                }
            };

            let mut handshake = match idevice::rsd::RsdHandshake::new(stream).await {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("Failed RSD handshake: {e:?}");
                    return Err(Errors::XpcHandshake);
                }
            };

            info!("Connecting to DVT via RSD");
            let pid = {
                let mut rs_client = match idevice::dvt::remote_server::RemoteServerClient::connect_rsd(
                    &mut handle,
                    &mut handshake,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("Failed to connect RemoteServerClient via RSD: {e:?}");
                        return Err(Errors::CreateRemoteServer);
                    }
                };

                let mut pc_client = match idevice::dvt::process_control::ProcessControlClient::new(
                    &mut rs_client,
                )
                .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("Failed to create process control client: {e:?}");
                        return Err(Errors::CreateProcessControl);
                    }
                };

                let pid = match pc_client.launch_app(app_id, None, None, true, false).await {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("Failed to launch app: {e:?}");
                        return Err(Errors::LaunchSuccess);
                    }
                };
                debug!("Launched app with PID {pid}");
                if let Err(e) = pc_client.disable_memory_limit(pid).await {
                    log::warn!("Failed to disable memory limit: {e:?}")
                }
                pid
            };

            info!("Connecting to debug proxy via RSD");
            let mut dp = match DebugProxyClient::connect_rsd(&mut handle, &mut handshake).await {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("Failed to connect to debug proxy via RSD: {e:?}");
                    return Err(Errors::CreateDebug);
                }
            };

            let commands = [
                format!("vAttach;{pid:02X}"),
                "D".to_string(),
                "D".to_string(),
                "D".to_string(),
                "D".to_string(),
            ];
            for command in commands {
                match dp.send_command(command.into()).await {
                    Ok(res) => {
                        debug!("command res: {res:?}");
                    }
                    Err(e) => {
                        log::warn!("Failed to send command to debug server: {e:?}");
                        return Err(Errors::Attach);
                    }
                }
            }
            Ok(())
        })
    }
}

/// Debugs an app from a process ID
/// # Arguments
/// - `pid`: Process ID. `attach_debugger` will automatically turn this into the format required by DebugServer.
pub fn attach_debugger(pid: u32) -> Res<()> {
    info!("Debugging process ID: {}", pid);

    if !test_device_connection() {
        error!("No device connection");
        return Err(Errors::NoConnection);
    }

    let device = fetch_first_device()?;

    let debug_server = match device.new_debug_server("minimuxer") {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to start debug server: {:?}", e);
            return Err(Errors::CreateDebug);
        }
    };

    // Taken from JitStreamer: https://github.com/jkcoxson/JitStreamer/blob/master/src/client.rs#L338-L363

    let command = "vAttach;";

    // The PID will consist of 8 hex digits, so we need to pad it with 0s
    let pid = format!("{pid:X}");
    println!("{pid}");
    let zeroes = 8 - pid.len();
    let pid = format!("{}{}", "0".repeat(zeroes), pid);
    let command = format!("{command}{pid}");
    info!("Sending command: {}", command);

    match debug_server.send_command(command.into()) {
        Ok(res) => info!("Successfully attached: {:?}", res),
        Err(e) => {
            error!("Error attaching: {:?}", e);
            return Err(Errors::Attach);
        }
    }

    match debug_server.send_command("D".into()) {
        Ok(res) => {
            info!("Success: {:?}", res);
            Ok(())
        }
        Err(e) => {
            error!("Error detaching: {:?}", e);
            Err(Errors::Detach)
        }
    }
}

// RPPairing (Remote Service Discovery) support for iOS 17+
// Modeled after SideStore/minimuxer RPPairing implementation

use idevice::{
    remote_pairing::{RemotePairingClient, RpPairingFile, RpPairingSocket},
    rsd::RsdHandshake,
    IdeviceError, RsdService,
};
use log::{error, info};
use std::{
    net::SocketAddrV4,
    str::FromStr,
    sync::{Mutex, OnceLock},
};

type RsdAdapter = idevice::tcp::handle::AdapterHandle;

struct CachedRsdConnection {
    adapter: RsdAdapter,
    handshake: RsdHandshake,
}

pub static RPPAIRING_FILE: OnceLock<RpPairingFile> = OnceLock::new();
static RPPAIRING_RSD_CONNECTION: OnceLock<Mutex<CachedRsdConnection>> = OnceLock::new();

/// Try to parse and store the pairing file bytes as an RPPairing file.
/// Returns Ok(()) on success, Err if the bytes are a classic lockdownd pairing file.
pub fn try_store_rppairing_file(pairing_file_bytes: &[u8]) -> Result<(), IdeviceError> {
    let pf = RpPairingFile::from_bytes(pairing_file_bytes)?;
    if RPPAIRING_FILE.set(pf).is_err() {
        info!("RPPairing file was already stored");
    } else {
        info!("RPPairing pairing file stored — iOS 17+ fallback available");
    }
    Ok(())
}

pub fn is_rppairing_available() -> bool {
    RPPAIRING_FILE.get().is_some()
}

pub async fn connect_to_rsd_services<Service: RsdService>() -> Result<Service, IdeviceError> {
    // Try existing cached connection first
    if let Some(conn_lock) = RPPAIRING_RSD_CONNECTION.get() {
        let mut guard = conn_lock.lock().unwrap();
        let conn = &mut *guard;
        match Service::connect_rsd(&mut conn.adapter, &mut conn.handshake).await {
            Ok(svc) => {
                info!("RPPairing: reusing cached RSD connection");
                return Ok(svc);
            }
            Err(IdeviceError::Socket(_)) => {
                info!("RPPairing: cached RSD connection dead, reconnecting");
            }
            Err(e) => return Err(e),
        }
    }

    // Create new connection and cache it
    let conn = create_rppairing_rsd_connection().await?;
    let mut guard;
    if let Some(old) = RPPAIRING_RSD_CONNECTION.get() {
        guard = old.lock().unwrap();
        guard.adapter = conn.adapter;
        guard.handshake = conn.handshake;
    } else {
        RPPAIRING_RSD_CONNECTION.set(Mutex::new(conn)).ok();
        guard = RPPAIRING_RSD_CONNECTION.get().unwrap().lock().unwrap();
    }
    let conn = &mut *guard;
    Service::connect_rsd(&mut conn.adapter, &mut conn.handshake).await
}

async fn create_rppairing_rsd_connection() -> Result<CachedRsdConnection, IdeviceError> {
    let mut pairing_file = match RPPAIRING_FILE.get() {
        Some(p) => p.clone(),
        None => {
            error!("RPPairing: no pairing file stored");
            return Err(IdeviceError::UserDeniedPairing);
        }
    };

    let device_ip = crate::muxer::DEVICE_IP
        .get()
        .map(|s| s.as_str())
        .unwrap_or("10.7.0.1");

    let socket_addr = SocketAddrV4::from_str(&format!("{}:49152", device_ip))
        .map_err(|_| IdeviceError::UserDeniedPairing)?;

    info!("RPPairing: connecting to {}:49152", device_ip);
    let stream = tokio::net::TcpStream::connect(socket_addr)
        .await
        .map_err(IdeviceError::Socket)?;

    let conn_sock = RpPairingSocket::new(stream);
    let mut rpc = RemotePairingClient::new(conn_sock, "minimuxer", &mut pairing_file);
    rpc.connect(async |_| "000000".to_string(), 0u8).await?;

    use idevice::remote_pairing::connect_tls_psk_tunnel_native;
    let tunnel_port = rpc.create_tcp_listener().await?;

    let tunnel_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(*socket_addr.ip()),
        tunnel_port,
    );
    let tunnel_stream = tokio::net::TcpStream::connect(tunnel_addr)
        .await
        .map_err(IdeviceError::Socket)?;
    let tunnel = connect_tls_psk_tunnel_native(tunnel_stream, rpc.encryption_key()).await?;

    let client_ip: std::net::IpAddr = tunnel
        .info
        .client_address
        .parse()
        .map_err(IdeviceError::AddrParseError)?;
    let server_ip: std::net::IpAddr = tunnel
        .info
        .server_address
        .parse()
        .map_err(IdeviceError::AddrParseError)?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;

    let raw = tunnel.into_inner();
    let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(raw), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60));
    let mut adapter = adapter.to_async_handle();

    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;

    info!("RPPairing: RSD connection established");
    Ok(CachedRsdConnection { adapter, handshake })
}

pub async fn install_provisioning_profile_rppairing(profile: &[u8]) -> Result<(), IdeviceError> {
    use idevice::misagent::MisagentClient;
    info!("RPPairing: installing provisioning profile via RSD misagent");
    let profile = profile.to_vec();
    let mut mis_client = connect_to_rsd_services::<MisagentClient>().await?;
    mis_client.install(profile).await
}

pub async fn remove_provisioning_profile_rppairing(id: String) -> Result<(), IdeviceError> {
    use idevice::misagent::MisagentClient;
    info!("RPPairing: removing provisioning profile via RSD misagent");
    let mut mis_client = connect_to_rsd_services::<MisagentClient>().await?;
    mis_client.remove(&id).await
}

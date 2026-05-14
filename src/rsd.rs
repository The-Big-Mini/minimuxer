// RPPairing (Remote Service Discovery) support for iOS 17+
//
// This module detects RPPairing-format pairing files (used on iOS 17+ devices
// that communicate via CoreDevice port 49152 rather than classic lockdownd).
//
// Full RPPairing tunnel implementation requires idevice 0.1.55+ remote_pairing
// support. For now, detection works but provisioning falls back to an error on
// iOS 26.5+ devices where classic misagent has been removed. A future upgrade
// to idevice 0.1.55+ will wire up the full tunnel.

use log::{error, info};
use once_cell::sync::OnceCell;

/// Stored bytes of the RPPairing file if the device uses CoreDevice / iOS 17+.
pub static RPPAIRING_FILE: OnceCell<Vec<u8>> = OnceCell::new();

/// Try to detect and store an RPPairing-format pairing file.
///
/// RPPairing files are plists that contain an `identifier` string and a
/// `private_key` data field, but NOT `DeviceCertificate` (which classic
/// lockdownd pairing files always have).
///
/// Returns `Ok(())` if the file looks like an RPPairing file, `Err(())` if it
/// looks like a classic lockdownd pairing file or is not parseable.
pub fn try_store_rppairing_file(pairing_file_bytes: &[u8]) -> Result<(), ()> {
    let plist: plist::Value = plist::from_bytes(pairing_file_bytes).map_err(|_| ())?;
    let dict = plist.as_dictionary().ok_or(())?;

    // RPPairing files have 'identifier' + 'private_key', classic files have 'DeviceCertificate'
    if dict.contains_key("identifier") && !dict.contains_key("DeviceCertificate") {
        if RPPAIRING_FILE.set(pairing_file_bytes.to_vec()).is_err() {
            info!("RPPairing file was already stored");
        } else {
            info!("RPPairing pairing file stored — iOS 17+ CoreDevice fallback active");
        }
        Ok(())
    } else {
        Err(())
    }
}

/// Returns true if a valid RPPairing file has been stored.
pub fn is_rppairing_available() -> bool {
    RPPAIRING_FILE.get().is_some()
}

/// Install a provisioning profile via the RPPairing / CoreDevice path.
///
/// NOTE: Full RPPairing tunnel support requires an idevice upgrade to 0.1.55+.
/// Until that upgrade is complete, this returns an error.
pub async fn install_provisioning_profile_rppairing(_profile: &[u8]) -> Result<(), crate::Errors> {
    error!(
        "RPPairing provisioning profile install is not yet implemented. \
         iOS 26.5 devices require a future minimuxer update."
    );
    Err(crate::Errors::ProfileInstall)
}

/// Remove a provisioning profile via the RPPairing / CoreDevice path.
///
/// NOTE: Full RPPairing tunnel support requires an idevice upgrade to 0.1.55+.
/// Until that upgrade is complete, this returns an error.
pub async fn remove_provisioning_profile_rppairing(_id: String) -> Result<(), crate::Errors> {
    error!(
        "RPPairing provisioning profile removal is not yet implemented. \
         iOS 26.5 devices require a future minimuxer update."
    );
    Err(crate::Errors::ProfileRemove)
}

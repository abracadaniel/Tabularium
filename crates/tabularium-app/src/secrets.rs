//! Password storage in the OS keychain (macOS Keychain, Windows Credential Manager,
//! Secret Service on Linux/BSD). Falls back gracefully when no keychain is available:
//! callers then keep the password in `connections.json`.

use uuid::Uuid;

#[cfg(feature = "keychain")]
const SERVICE: &str = "dev.tabularium.Tabularium";

#[cfg(feature = "keychain")]
fn entry(id: Uuid) -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, &id.to_string()).ok()
}

/// Store a password; returns `false` when no keychain is usable.
pub fn store(id: Uuid, password: &str) -> bool {
    #[cfg(feature = "keychain")]
    {
        match entry(id).map(|e| e.set_password(password)) {
            Some(Ok(())) => return true,
            Some(Err(e)) => log::warn!("keychain unavailable: {e}"),
            None => {}
        }
    }
    let _ = (id, password);
    false
}

pub fn load(id: Uuid) -> Option<String> {
    #[cfg(feature = "keychain")]
    {
        return entry(id)?.get_password().ok();
    }
    #[allow(unreachable_code)]
    {
        let _ = id;
        None
    }
}

pub fn delete(id: Uuid) {
    #[cfg(feature = "keychain")]
    if let Some(e) = entry(id) {
        let _ = e.delete_credential();
    }
    let _ = id;
}

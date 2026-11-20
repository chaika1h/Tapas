//! Passwords at rest.
//!
//! On Windows, secrets are encrypted with DPAPI (`CryptProtectData`), which
//! ties them to the signed-in Windows user: copying `config.json` to another
//! account or PC yields nothing usable. On other platforms (development only)
//! they are merely base64-encoded.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(plain: &str) -> Self {
        if plain.is_empty() {
            return Self::default();
        }
        match protect(plain.as_bytes()) {
            Ok(s) => Self(s),
            Err(e) => {
                log::error!("could not encrypt secret: {e:#}");
                Self::default()
            }
        }
    }

    pub fn reveal(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        match unprotect(&self.0) {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
            Err(e) => {
                log::error!("could not decrypt a saved password ({e:#}); please re-enter it");
                String::new()
            }
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "Secret(<empty>)" } else { "Secret(<redacted>)" })
    }
}

#[cfg(windows)]
fn protect(data: &[u8]) -> anyhow::Result<String> {
    Ok(format!("dpapi:{}", B64.encode(dpapi::protect(data)?)))
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> anyhow::Result<String> {
    Ok(format!("plain:{}", B64.encode(data)))
}

fn unprotect(stored: &str) -> anyhow::Result<Vec<u8>> {
    let (scheme, payload) = stored.split_once(':').unwrap_or(("", stored));
    let raw = B64.decode(payload)?;
    match scheme {
        #[cfg(windows)]
        "dpapi" => dpapi::unprotect(&raw),
        "plain" => Ok(raw),
        other => anyhow::bail!("unsupported secret scheme {other:?} (saved on another OS?)"),
    }
}

#[cfg(windows)]
mod dpapi {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies DPAPI's output and frees it with `LocalFree`, as the API requires.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        v
    }

    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        // SAFETY: all pointers are valid for the duration of the call.
        let ok = unsafe {
            CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            anyhow::bail!("CryptProtectData: {}", std::io::Error::last_os_error());
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        // SAFETY: as above.
        let ok = unsafe {
            CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            anyhow::bail!("CryptUnprotectData: {}", std::io::Error::last_os_error());
        }
        Ok(unsafe { take(out) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Secret::new("hunter2 ünïcode");
        assert_eq!(s.reveal(), "hunter2 ünïcode");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert_eq!(Secret::new("").reveal(), "");
    }
}

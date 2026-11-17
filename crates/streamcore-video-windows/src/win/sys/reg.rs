//! Read-only registry access: the Media Foundation encoder registrations (the bench's `encoders`
//! diagnostics, and the direct-creation fallback when enumeration lists no hardware encoder).

use crate::model::RegistryValue;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, KEY_READ, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_QWORD, REG_SZ, REG_VALUE_TYPE,
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::{GUID, HSTRING, PWSTR};

/// `MFT_CATEGORY_VIDEO_ENCODER` under `HKEY_CLASSES_ROOT`, as the registry spells it (no braces,
/// like its subkeys).
pub const ENCODER_CATEGORY: &str =
    r"MediaFoundation\Transforms\Categories\f79eac7d-e545-4387-bdee-d647d7bde42a";

pub struct Key(HKEY);

impl Key {
    pub fn open(root: HKEY, path: &str) -> Option<Self> {
        let mut key = HKEY::default();
        // SAFETY: valid root key and path; the out pointer is a valid local, closed in Drop.
        let r = unsafe { RegOpenKeyExW(root, &HSTRING::from(path), None, KEY_READ, &mut key) };
        (r == ERROR_SUCCESS).then_some(Self(key))
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: valid key; the name buffer and its length are valid locals.
            let r = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        out
    }

    pub fn values(&self) -> Vec<RegistryValue> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut data = vec![0u8; 4096];
            let mut data_len = data.len() as u32;
            // SAFETY: valid key; every buffer and length is a valid local.
            let r = unsafe {
                RegEnumValueW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut kind),
                    Some(data.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            data.truncate(data_len as usize);
            out.push(RegistryValue {
                name: String::from_utf16_lossy(&name[..name_len as usize]),
                value: render(REG_VALUE_TYPE(kind), &data),
            });
        }
        out
    }

    pub fn value(&self, name: &str) -> Option<String> {
        let mut kind = REG_VALUE_TYPE(0);
        let mut data = vec![0u8; 4096];
        let mut len = data.len() as u32;
        // SAFETY: valid key; the buffers are valid locals.
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                &HSTRING::from(name),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut len),
            )
        };
        (r == ERROR_SUCCESS).then(|| {
            data.truncate(len as usize);
            render(kind, &data)
        })
    }
}

impl Key {
    /// A `REG_DWORD` value.
    pub fn dword(&self, name: &str) -> Option<u32> {
        let mut kind = REG_VALUE_TYPE(0);
        let mut data = [0u8; 4];
        let mut len = data.len() as u32;
        // SAFETY: valid key; the buffers are valid locals.
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                &HSTRING::from(name),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut len),
            )
        };
        (r == ERROR_SUCCESS && kind == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this struct opened.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn render(kind: REG_VALUE_TYPE, data: &[u8]) -> String {
    match kind {
        REG_DWORD if data.len() >= 4 => {
            let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            format!("0x{v:x} ({v})")
        }
        REG_QWORD if data.len() >= 8 => {
            format!(
                "0x{:x}",
                u64::from_le_bytes(data[..8].try_into().unwrap_or([0; 8]))
            )
        }
        REG_SZ | REG_EXPAND_SZ => {
            let wide: Vec<u16> = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16_lossy(&wide)
        }
        REG_BINARY => format!("{} bytes", data.len()),
        other => format!("type {} ({} bytes)", other.0, data.len()),
    }
}

/// The registry spelling of a GUID, with or without braces.
pub fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

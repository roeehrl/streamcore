//! Encoder diagnostics (the bench's `encoders` command): what Media Foundation has registered as
//! video encoders, what each enumeration call returns for them (with and without the hardware,
//! adapter and sort-and-filter options), whether each can be activated or created directly, and
//! the Media Foundation registry settings that change enumeration. Read-only, apart from creating
//! and releasing the encoders it tests.

use super::d3d::u64_to_luid;
use crate::model::{EnumCall, MftEntry, RegisteredMft, RegistryValue};
use std::ffi::c_void;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ, REG_BINARY, REG_DWORD, REG_EXPAND_SZ,
    REG_QWORD, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows::core::{GUID, HSTRING, Interface, PWSTR};

/// `MFT_CATEGORY_VIDEO_ENCODER` as the registry spells it (no braces, like its subkeys).
const ENCODER_CATEGORY: &str =
    r"MediaFoundation\Transforms\Categories\f79eac7d-e545-4387-bdee-d647d7bde42a";

fn guid_string(g: &GUID) -> String {
    format!("{{{g:?}}}").to_lowercase()
}

fn subtype_name(g: &GUID) -> String {
    let known = [
        (MFVideoFormat_NV12, "NV12"),
        (MFVideoFormat_H264, "H264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_AV1, "AV1"),
        (MFVideoFormat_ARGB32, "ARGB32"),
        (MFVideoFormat_RGB32, "RGB32"),
        (MFVideoFormat_YUY2, "YUY2"),
        (MFVideoFormat_IYUV, "IYUV"),
        (MFVideoFormat_P010, "P010"),
    ];
    known
        .iter()
        .find(|(k, _)| k == g)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| guid_string(g))
}

/// The H.264 output subtype, for callers outside `sys`.
pub fn h264() -> GUID {
    MFVideoFormat_H264
}

pub fn nv12() -> GUID {
    MFVideoFormat_NV12
}

fn attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: valid attribute store; on success MF allocates the string, copied and freed here.
    unsafe {
        attrs.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

fn entry(activate: &IMFActivate, try_activate: bool) -> MftEntry {
    let mut e = MftEntry::default();
    if let Ok(attrs) = activate.cast::<IMFAttributes>() {
        e.name = attr_string(&attrs, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
        // SAFETY: valid attribute store; plain value reads.
        unsafe {
            e.clsid = attrs
                .GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
                .ok()
                .map(|g| guid_string(&g));
            e.flags = attrs.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute).ok();
            e.merit = attrs.GetUINT32(&MFT_CODEC_MERIT_Attribute).ok();
            let mut luid = [0u8; 8];
            if attrs
                .GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut luid, None)
                .is_ok()
            {
                e.adapter_luid = Some(format!("{:016x}", u64::from_le_bytes(luid)));
            }
        }
        e.hardware_url = attr_string(&attrs, &MFT_ENUM_HARDWARE_URL_Attribute);
        e.vendor_id = attr_string(&attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
    }
    if try_activate {
        // SAFETY: valid activation object; the MFT is shut down right after.
        e.activation = Some(match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(_mft) => {
                // SAFETY: as above; releases the MFT and any hardware session.
                let _ = unsafe { activate.ShutdownObject() };
                "ok".into()
            }
            Err(err) => format!("{err}"),
        });
    }
    e
}

/// One enumeration call: `MFTEnum2` (scoped to `luid` when given) or `MFTEnumEx`.
pub fn enumerate(
    label: &str,
    enum2: bool,
    flags: MFT_ENUM_FLAG,
    input: Option<GUID>,
    output: Option<GUID>,
    luid: Option<u64>,
    try_activate: bool,
) -> EnumCall {
    let mut call = EnumCall {
        label: label.into(),
        api: if enum2 { "MFTEnum2" } else { "MFTEnumEx" }.into(),
        flags: flags.0 as u32,
        input: input.as_ref().map(subtype_name),
        output: output.as_ref().map(subtype_name),
        adapter_luid: luid.map(|l| format!("{l:016x}")),
        ..EnumCall::default()
    };
    let input = input.map(|subtype| MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    });
    let output = output.map(|subtype| MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    });
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let result = if enum2 {
        let mut attrs: Option<IMFAttributes> = None;
        if let Some(l) = luid {
            let luid = u64_to_luid(l);
            // SAFETY: creates an attribute store; the out pointer is a valid local.
            if let Err(e) = unsafe { MFCreateAttributes(&mut attrs, 1) } {
                call.error = Some(format!("MFCreateAttributes: {e}"));
                return call;
            }
            // SAFETY: LUID is a plain 8-byte struct, read for the call only; SetBlob copies it.
            let set = unsafe {
                let bytes = std::slice::from_raw_parts(
                    (&raw const luid).cast::<u8>(),
                    std::mem::size_of_val(&luid),
                );
                attrs
                    .as_ref()
                    .map(|a| a.SetBlob(&MFT_ENUM_ADAPTER_LUID, bytes))
            };
            if let Some(Err(e)) = set {
                call.error = Some(format!("SetBlob(MFT_ENUM_ADAPTER_LUID): {e}"));
                return call;
            }
        }
        // SAFETY: the type infos and out pointers are valid locals; MF allocates the array.
        unsafe {
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                input.as_ref().map(|i| i as *const _),
                output.as_ref().map(|o| o as *const _),
                attrs.as_ref(),
                &mut array,
                &mut count,
            )
        }
    } else {
        // SAFETY: as above.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                input.as_ref().map(|i| i as *const _),
                output.as_ref().map(|o| o as *const _),
                &mut array,
                &mut count,
            )
        }
    };
    if let Err(e) = result {
        call.error = Some(format!("{e}"));
        return call;
    }
    if array.is_null() {
        return call;
    }
    // SAFETY: the call returned `count` activation pointers in a CoTaskMemAlloc'd array; each
    // element's reference is taken and the array is freed once.
    unsafe {
        for slot in std::slice::from_raw_parts_mut(array, count as usize) {
            if let Some(activate) = slot.take() {
                call.found.push(entry(&activate, try_activate));
            }
        }
        CoTaskMemFree(Some(array as *const c_void));
    }
    call
}

struct Key(HKEY);

impl Key {
    fn open(root: HKEY, path: &str) -> Option<Self> {
        let mut key = HKEY::default();
        // SAFETY: valid root key and path; the out pointer is a valid local, closed in Drop.
        let r = unsafe { RegOpenKeyExW(root, &HSTRING::from(path), None, KEY_READ, &mut key) };
        (r == ERROR_SUCCESS).then_some(Self(key))
    }

    fn subkeys(&self) -> Vec<String> {
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

    fn values(&self) -> Vec<RegistryValue> {
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

    fn value(&self, name: &str) -> Option<String> {
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
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16_lossy(&wide)
        }
        REG_BINARY => format!("{} bytes", data.len()),
        other => format!("type {} ({} bytes)", other.0, data.len()),
    }
}

/// Media Foundation's registry settings that bear on encoder enumeration: every value under
/// `HKLM\SOFTWARE\Microsoft\Windows Media Foundation` and its direct subkeys (as `subkey\name`).
pub fn mf_settings() -> Vec<RegistryValue> {
    let base = r"SOFTWARE\Microsoft\Windows Media Foundation";
    let Some(root) = Key::open(HKEY_LOCAL_MACHINE, base) else {
        return vec![RegistryValue {
            name: base.into(),
            value: "(missing)".into(),
        }];
    };
    let mut out = root.values();
    for sub in root.subkeys() {
        if let Some(key) = Key::open(HKEY_LOCAL_MACHINE, &format!(r"{base}\{sub}")) {
            for v in key.values() {
                out.push(RegistryValue {
                    name: format!(r"{sub}\{}", v.name),
                    value: v.value,
                });
            }
        }
    }
    out
}

fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

/// Every video encoder registered in the encoder category, with its registration and whether
/// `CoCreateInstance` gives an `IMFTransform` (a hardware MFT normally comes through enumeration
/// and activation instead; this shows whether its DLL loads at all).
pub fn registered() -> Vec<RegisteredMft> {
    let Some(category) = Key::open(HKEY_CLASSES_ROOT, ENCODER_CATEGORY) else {
        return Vec::new();
    };
    category
        .subkeys()
        .into_iter()
        .map(|clsid| {
            let mut m = RegisteredMft {
                clsid: clsid.to_lowercase(),
                ..RegisteredMft::default()
            };
            if let Some(key) = Key::open(
                HKEY_CLASSES_ROOT,
                &format!(r"MediaFoundation\Transforms\{clsid}"),
            ) {
                m.name = key.value("").unwrap_or_default();
                m.mft_flags = key.value("MFTFlags");
                m.attributes = key.value("Attributes");
            }
            if let Some(key) = Key::open(
                HKEY_CLASSES_ROOT,
                &format!(r"CLSID\{{{clsid}}}\InprocServer32"),
            ) {
                m.dll = key.value("");
                m.threading = key.value("ThreadingModel");
            }
            m.create = Some(match parse_guid(&clsid) {
                // SAFETY: a registered CLSID; the object is released when it drops here.
                Some(g) => match unsafe {
                    CoCreateInstance::<_, IMFTransform>(&g, None, CLSCTX_INPROC_SERVER)
                } {
                    Ok(mft) => {
                        // A hardware MFT created this way still needs its D3D manager; only the
                        // attribute store is read.
                        // SAFETY: valid MFT.
                        if let Ok(attrs) = unsafe { mft.GetAttributes() } {
                            m.hardware_url = attr_string(&attrs, &MFT_ENUM_HARDWARE_URL_Attribute);
                            // SAFETY: valid attribute store.
                            m.d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }
                                .ok()
                                .map(|v| v != 0);
                        }
                        drop(mft);
                        "ok".into()
                    }
                    Err(e) => format!("{e}"),
                },
                None => "unparsable CLSID".into(),
            });
            m
        })
        .collect()
}

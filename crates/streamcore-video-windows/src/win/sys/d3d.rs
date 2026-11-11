//! DXGI adapter and output enumeration, the D3D11 device on a chosen adapter, NV12 textures, the
//! convert fence, barcode readback and the WinRT interop for WGC.

use super::com::Event;
use std::ffi::c_void;
use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_NONE, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice,
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAS_STILL_DRAWING, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIFactory4,
    IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, HMONITOR,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{Interface, PCWSTR, Result};

pub fn luid_to_u64(l: LUID) -> u64 {
    (u64::from(l.HighPart as u32) << 32) | u64::from(l.LowPart)
}

pub fn u64_to_luid(v: u64) -> LUID {
    LUID {
        LowPart: v as u32,
        HighPart: (v >> 32) as i32,
    }
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|c| *c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

#[derive(Clone, Debug)]
pub struct OutputDesc {
    pub device_name: String,
    pub desktop: [i32; 4],
    pub attached: bool,
    /// HMONITOR value (process-local handle) for WGC's `CreateForMonitor`.
    pub hmonitor: usize,
    pub bits_per_color: u32,
    pub color_space: i32,
    pub refresh_hz: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct AdapterDesc {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid: u64,
    pub dedicated_video_memory: u64,
    pub shared_system_memory: u64,
    pub software: bool,
    pub outputs: Vec<OutputDesc>,
}

fn refresh_hz(device_name: &[u16]) -> Option<f64> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..DEVMODEW::default()
    };
    // SAFETY: `device_name` is the NUL-terminated name from DXGI_OUTPUT_DESC; `mode` is a valid
    // DEVMODEW with dmSize set.
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    (ok.as_bool() && mode.dmDisplayFrequency > 1).then_some(f64::from(mode.dmDisplayFrequency))
}

/// Every DXGI adapter with its outputs, in enumeration order.
pub fn enumerate_adapters() -> Result<Vec<AdapterDesc>> {
    // SAFETY: creates a factory object; no pointers involved.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    for index in 0.. {
        // SAFETY: valid factory; DXGI_ERROR_NOT_FOUND ends the list.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e),
        };
        // SAFETY: valid adapter; returns a plain struct.
        let desc = unsafe { adapter.GetDesc1()? };
        let mut outputs = Vec::new();
        for o in 0.. {
            // SAFETY: valid adapter; DXGI_ERROR_NOT_FOUND ends the list.
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            // SAFETY: valid output; returns a plain struct.
            let d = unsafe { output.GetDesc()? };
            let (bits, cs) = match output.cast::<IDXGIOutput6>() {
                // SAFETY: valid IDXGIOutput6; returns a plain struct.
                Ok(o6) => unsafe { o6.GetDesc1() }
                    .map(|d1| (d1.BitsPerColor, d1.ColorSpace.0))
                    .unwrap_or((0, -1)),
                Err(_) => (0, -1),
            };
            let r = d.DesktopCoordinates;
            outputs.push(OutputDesc {
                device_name: wide(&d.DeviceName),
                desktop: [r.left, r.top, r.right, r.bottom],
                attached: d.AttachedToDesktop.as_bool(),
                hmonitor: d.Monitor.0 as usize,
                bits_per_color: bits,
                color_space: cs,
                refresh_hz: refresh_hz(&d.DeviceName),
            });
        }
        out.push(AdapterDesc {
            index,
            name: wide(&desc.Description),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            luid: luid_to_u64(desc.AdapterLuid),
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
            shared_system_memory: desc.SharedSystemMemory as u64,
            software: desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
            outputs,
        });
    }
    Ok(out)
}

/// The D3D11 device every stage of a stream uses: capture (the WGC frame pool), convert and the
/// encoder MFT (through its DXGI device manager). The immediate context is used from the
/// executor, the fence thread and the MFT's threads, so the device is multithread-protected
/// (`ID3D11Multithread::SetMultithreadProtected`): every context call takes the device lock.
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub luid: u64,
    pub name: String,
}

impl Gpu {
    /// Creates a device on the adapter with `luid` (or the first adapter), with BGRA and video
    /// support, multithread-protected because the encoder MFT uses it from its own threads.
    pub fn create(luid: Option<u64>) -> Result<Self> {
        // SAFETY: creates a factory object; no pointers involved.
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1()? };
        let adapter: IDXGIAdapter1 = match luid {
            // SAFETY: valid factory; plain LUID argument.
            Some(l) => unsafe { factory.EnumAdapterByLuid(u64_to_luid(l))? },
            // SAFETY: valid factory.
            None => unsafe { factory.EnumAdapters1(0)? },
        };
        // SAFETY: valid adapter; returns a plain struct.
        let desc = unsafe { adapter.GetDesc1()? };
        let mut device = None;
        let mut context = None;
        // SAFETY: out pointers are valid locals; the feature-level slice lives across the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let context = context.ok_or_else(windows::core::Error::empty)?;
        let mt: ID3D11Multithread = device.cast()?;
        // SAFETY: valid interface; plain BOOL argument. Must hold before the device is shared.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
        // SAFETY: as above; reads the flag back.
        if !unsafe { mt.GetMultithreadProtected() }.as_bool() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "device is not multithread-protected",
            ));
        }
        Ok(Self {
            device,
            context,
            adapter,
            luid: luid_to_u64(desc.AdapterLuid),
            name: wide(&desc.Description),
        })
    }

    /// The device as WinRT `IDirect3DDevice` for the WGC frame pool.
    pub fn winrt_device(&self) -> Result<IDirect3DDevice> {
        let dxgi: IDXGIDevice = self.device.cast()?;
        // SAFETY: valid DXGI device; returns an owned WinRT object.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        inspectable.cast()
    }

    /// Creates an NV12 texture (optionally filled with `initial`, Y plane then UV plane, pitch
    /// `width`).
    pub fn create_nv12(
        &self,
        width: u32,
        height: u32,
        bind: D3D11_BIND_FLAG,
        initial: Option<&[u8]>,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let expected = (width as usize) * (height as usize) * 3 / 2;
        let data = initial
            .filter(|d| d.len() >= expected)
            .map(|d| D3D11_SUBRESOURCE_DATA {
                pSysMem: d.as_ptr().cast::<c_void>(),
                SysMemPitch: width,
                SysMemSlicePitch: 0,
            });
        let mut tex = None;
        // SAFETY: `desc` and `data` (whose buffer was checked to cover the NV12 surface) live
        // across the call; the out pointer is a valid local.
        unsafe {
            self.device.CreateTexture2D(
                &desc,
                data.as_ref().map(|d| d as *const _),
                Some(&mut tex),
            )?
        };
        tex.ok_or_else(windows::core::Error::empty)
    }

    pub fn flush(&self) {
        // SAFETY: valid immediate context.
        unsafe { self.context.Flush() };
    }
}

/// The WGC capture item for a monitor.
pub fn capture_item_for_monitor(hmonitor: usize) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: `hmonitor` came from DXGI_OUTPUT_DESC in this process; the call validates it.
    unsafe { interop.CreateForMonitor(HMONITOR(hmonitor as *mut c_void)) }
}

/// The D3D11 texture behind a WGC frame's surface.
pub fn texture_from_surface(surface: &IDirect3DSurface) -> Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    // SAFETY: valid interop interface; returns an owned COM pointer of the requested type.
    unsafe { access.GetInterface() }
}

/// Width and height of a texture.
pub fn texture_size(tex: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; `desc` is a valid out pointer.
    unsafe { tex.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

/// The convert fence (`GpuFence::D3d11Fence` in the design doc).
pub struct Fence {
    fence: ID3D11Fence,
    context: ID3D11DeviceContext4,
}

impl Fence {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let device5: ID3D11Device5 = gpu.device.cast()?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: valid device; the out pointer is a valid local.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)? };
        Ok(Self {
            fence: fence.ok_or_else(windows::core::Error::empty)?,
            context: gpu.context.cast()?,
        })
    }

    /// Queues a signal of `value` after the GPU work recorded so far.
    pub fn signal(&self, value: u64) -> Result<()> {
        // SAFETY: valid context and fence.
        unsafe { self.context.Signal(&self.fence, value) }
    }

    pub fn completed(&self) -> u64 {
        // SAFETY: valid fence.
        unsafe { self.fence.GetCompletedValue() }
    }

    /// Sets `event` when the fence reaches `value`.
    pub fn notify(&self, value: u64, event: &Event) -> Result<()> {
        // SAFETY: valid fence; the caller keeps `event` alive until it is signalled or the fence
        // is dropped.
        unsafe { self.fence.SetEventOnCompletion(value, event.raw()) }
    }

    /// Makes GPU work queued after this call wait until the fence reaches `value` (no CPU wait).
    pub fn gpu_wait(&self, value: u64) -> Result<()> {
        // SAFETY: valid context and fence.
        unsafe { self.context.Wait(&self.fence, value) }
    }
}

/// A two-row NV12 staging texture for reading the barcode strip back without stalling.
pub struct Readback {
    staging: ID3D11Texture2D,
    width: u32,
}

impl Readback {
    pub fn new(gpu: &Gpu, width: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: valid device, descriptor and out pointer.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(Self {
            staging: tex.ok_or_else(windows::core::Error::empty)?,
            width,
        })
    }

    /// Records a copy of two rows starting at even row `y` of `src` (an NV12 texture as wide).
    pub fn copy_rows(&self, gpu: &Gpu, src: &ID3D11Texture2D, y: u32) {
        let src_box = D3D11_BOX {
            left: 0,
            top: y & !1,
            front: 0,
            right: self.width,
            bottom: (y & !1) + 2,
            back: 1,
        };
        // SAFETY: both textures are NV12 of the same width on this device; the box is even
        // aligned and inside the source (the caller passes y + 2 <= source height).
        unsafe {
            gpu.context
                .CopySubresourceRegion(&self.staging, 0, 0, 0, 0, src, 0, Some(&src_box))
        };
    }

    /// The first luma row if the copy has finished, without waiting.
    pub fn try_read_row(&self, gpu: &Gpu) -> Result<Option<Vec<u8>>> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: valid staging texture; `mapped` is a valid out pointer.
        let r = unsafe {
            gpu.context.Map(
                &self.staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        };
        match r {
            Ok(()) => {
                // SAFETY: Map succeeded, so pData points at the Y plane with at least `width`
                // readable bytes in row 0; the slice is copied before Unmap.
                let row = unsafe {
                    std::slice::from_raw_parts(mapped.pData.cast::<u8>(), self.width as usize)
                }
                .to_vec();
                // SAFETY: balances the successful Map.
                unsafe { gpu.context.Unmap(&self.staging, 0) };
                Ok(Some(row))
            }
            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => Ok(None),
            Err(e) => Err(e),
        }
    }
}

//! Media Foundation encoder MFTs: enumeration per adapter (`MFTEnum2` with the adapter LUID),
//! configuration for low latency with every `ICodecAPI` value read back, DXGI-surface input, and
//! the asynchronous event pump whose thread collects output as soon as the MFT reports it (R1).

use super::com::now_ns;
use super::d3d::{Gpu, u64_to_luid};
use crate::model::{CodecSetting, EncoderInfo, EncoderSetup};
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{E_FAIL, E_UNEXPECTED, VARIANT_BOOL, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VT_BOOL, VT_UI4, VariantClear};
use windows::core::{Error, GUID, HRESULT, Interface, PWSTR, Result};

fn attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: valid attribute store; on success MF allocates the string, which is copied and
    // freed here.
    unsafe {
        attrs.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// `D3DDDIERR_DEVICEREMOVED`, which some drivers' MFTs return as is.
const D3DDDIERR_DEVICEREMOVED: HRESULT = HRESULT(0x8876_0870_u32 as i32);

/// Whether an MFT call failed because the GPU device was removed, reset or hung: the session is
/// gone and the stream rebuilds the encoder (`MediaError::DeviceLost`).
pub fn is_device_lost(code: HRESULT) -> bool {
    [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DEVICE_HUNG,
        D3DDDIERR_DEVICEREMOVED,
    ]
    .contains(&code)
}

/// `ProcessInput` refused the input because the MFT is not accepting one (no `NeedInput`
/// credit, as the MFT sees it): the credit the caller took was not real.
pub fn is_not_accepting(e: &Error) -> bool {
    e.code() == MF_E_NOTACCEPTING
}

/// Shuts an activated MFT down when dropped, unless disarmed: every early return between
/// `ActivateObject` and a configured encoder would otherwise leak the session, and hardware
/// encoders limit sessions per process.
struct ActivationGuard(Option<IMFActivate>);

impl ActivationGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        if let Some(activate) = self.0.take() {
            // SAFETY: valid activation object whose MFT was created; releases the MFT and its
            // hardware session.
            let _ = unsafe { activate.ShutdownObject() };
        }
    }
}

/// An encoder MFT `MFTEnum2` returned, not yet activated.
pub struct Candidate {
    pub info: EncoderInfo,
    activate: IMFActivate,
}

/// H.264 encoder MFTs for NV12 input: the hardware ones scoped to the adapter `luid`, or the
/// synchronous software ones.
pub fn enumerate_h264(hardware: bool, luid: Option<u64>) -> Result<Vec<Candidate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let mut attrs: Option<IMFAttributes> = None;
    if let Some(l) = luid {
        let luid = u64_to_luid(l);
        // SAFETY: creates an attribute store; the out pointer is a valid local.
        unsafe { MFCreateAttributes(&mut attrs, 1)? };
        let a = attrs.as_ref().ok_or_else(Error::empty)?;
        // SAFETY: LUID is a plain 8-byte struct; the bytes are read for the call only.
        let bytes = unsafe {
            std::slice::from_raw_parts((&raw const luid).cast::<u8>(), std::mem::size_of_val(&luid))
        };
        // SAFETY: valid attribute store; SetBlob copies the bytes.
        unsafe { a.SetBlob(&MFT_ENUM_ADAPTER_LUID, bytes)? };
    }
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the type-info structs and out pointers are valid locals; MF allocates the array.
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            attrs.as_ref(),
            &mut array,
            &mut count,
        )?;
    }
    let mut out = Vec::new();
    if array.is_null() {
        return Ok(out);
    }
    // SAFETY: MFTEnum2 returned `count` activation pointers in a CoTaskMemAlloc'd array; each
    // element's reference is taken (leaving None behind) and the array is freed once.
    unsafe {
        for slot in std::slice::from_raw_parts_mut(array, count as usize) {
            if let Some(activate) = slot.take() {
                let info = match activate.cast::<IMFAttributes>() {
                    Ok(attrs) => EncoderInfo {
                        name: attr_string(&attrs, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default(),
                        codec: "h264".into(),
                        hardware,
                        vendor_id: attr_string(&attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
                    },
                    Err(_) => EncoderInfo::default(),
                };
                out.push(Candidate { info, activate });
            }
        }
        CoTaskMemFree(Some(array as *const _));
    }
    Ok(out)
}

/// Encoder parameters.
#[derive(Clone, Debug)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

#[derive(Clone, Copy)]
enum Val {
    U32(u32),
    Bool(bool),
}

impl Val {
    fn text(self) -> String {
        match self {
            Val::U32(v) => v.to_string(),
            Val::Bool(b) => b.to_string(),
        }
    }
}

fn variant(v: Val) -> VARIANT {
    let mut var = VARIANT::default();
    let mut inner = VARIANT_0_0::default();
    match v {
        Val::U32(x) => {
            inner.vt = VT_UI4;
            inner.Anonymous.ulVal = x;
        }
        Val::Bool(b) => {
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = if b { VARIANT_TRUE } else { VARIANT_BOOL(0) };
        }
    }
    var.Anonymous.Anonymous = ManuallyDrop::new(inner);
    var
}

fn read_variant(var: &VARIANT) -> Option<String> {
    // SAFETY: reads the tag, then only the union member the tag names.
    unsafe {
        let inner = &var.Anonymous.Anonymous;
        match inner.vt {
            VT_UI4 => Some(inner.Anonymous.ulVal.to_string()),
            VT_BOOL => Some((inner.Anonymous.boolVal.0 != 0).to_string()),
            other => Some(format!("vt={}", other.0)),
        }
    }
}

/// One encoder input: a sample wrapping an NV12 texture.
pub struct Sample(IMFSample);

// SAFETY: samples and DXGI surface buffers from MFCreateSample/MFCreateDXGISurfaceBuffer are
// free-threaded Media Foundation objects; a held input is created on the executor and handed,
// once, to the event thread that feeds it.
unsafe impl Send for Sample {}

/// An activated, configured, streaming asynchronous encoder MFT.
pub struct Encoder {
    activate: IMFActivate,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    events: IMFMediaEventGenerator,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_buffer_size: u32,
    pub setup: EncoderSetup,
    /// Set by `shutdown`: a `HaveOutput` the pump reads afterwards is teardown, not an error.
    closing: Arc<AtomicBool>,
    // Kept alive for the encoder's lifetime.
    _manager: IMFDXGIDeviceManager,
}

// SAFETY: hardware encoder MFTs are asynchronous MFTs, which are free-threaded by contract: the
// client calls ProcessInput and ProcessOutput from different threads in response to events
// (MF "Asynchronous MFTs"). The executor calls ProcessInput, ProcessMessage and ICodecAPI; the
// event thread calls GetEvent, ProcessInput (a held input) and ProcessOutput. ProcessInput calls
// are serialised by the caller.
unsafe impl Send for Encoder {}
// SAFETY: as above; every method takes `&self` and only calls the MFT.
unsafe impl Sync for Encoder {}

impl Encoder {
    /// Activates `candidate` on `gpu`'s device and configures it for low-latency CBR H.264 Main,
    /// no B frames, an IDR only on request. Rejects synchronous MFTs.
    pub fn create(candidate: &Candidate, gpu: &Gpu, p: &Params) -> Result<Self> {
        let mut setup = EncoderSetup {
            name: candidate.info.name.clone(),
            hardware: candidate.info.hardware,
            width: p.width,
            height: p.height,
            fps: p.fps,
            bitrate: u64::from(p.bitrate),
            ..EncoderSetup::default()
        };
        let activate = candidate.activate.clone();
        // SAFETY: valid activation object; creates the MFT.
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };
        let guard = ActivationGuard(Some(activate.clone()));
        // SAFETY: valid MFT; attribute calls take valid GUID pointers.
        let attrs = unsafe { transform.GetAttributes()? };
        // SAFETY: as above.
        setup.asynchronous = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
        let events: Option<IMFMediaEventGenerator> = transform.cast().ok();
        let Some(events) = events.filter(|_| setup.asynchronous) else {
            return Err(Error::new(E_FAIL, "not an asynchronous MFT"));
        };
        // SAFETY: as above; unlocking is required before using an async MFT.
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)? };
        // SAFETY: as above.
        if let Err(e) = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) } {
            setup.notes.push(format!("MF_LOW_LATENCY attribute: {e}"));
        }
        // SAFETY: as above.
        setup.d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 1;

        let (mut input_id, mut output_id) = (0u32, 0u32);
        let mut ins = [0u32; 1];
        let mut outs = [0u32; 1];
        // SAFETY: valid MFT; slices sized for one stream each. E_NOTIMPL means IDs 0 and 0.
        if unsafe { transform.GetStreamIDs(&mut ins, &mut outs) }.is_ok() {
            input_id = ins[0];
            output_id = outs[0];
        }

        let mut manager = None;
        let mut token = 0u32;
        // SAFETY: out pointers are valid locals.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager: IMFDXGIDeviceManager = manager.ok_or_else(Error::empty)?;
        // SAFETY: valid manager and device; the token came from MFCreateDXGIDeviceManager.
        unsafe { manager.ResetDevice(&gpu.device, token)? };
        // SAFETY: the MFT AddRefs the manager it is given; this struct keeps its own reference.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?
        };

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        let wanted: Vec<(&str, GUID, Val)> = vec![
            (
                "AVLowLatencyMode",
                CODECAPI_AVLowLatencyMode,
                Val::Bool(true),
            ),
            (
                "AVEncCommonRateControlMode",
                CODECAPI_AVEncCommonRateControlMode,
                Val::U32(eAVEncCommonRateControlMode_CBR.0 as u32),
            ),
            (
                "AVEncCommonMeanBitRate",
                CODECAPI_AVEncCommonMeanBitRate,
                Val::U32(p.bitrate),
            ),
            (
                "AVEncMPVDefaultBPictureCount",
                CODECAPI_AVEncMPVDefaultBPictureCount,
                Val::U32(0),
            ),
            // 0: no periodic IDR; the stream asks for one when it needs it.
            ("AVEncMPVGOPSize", CODECAPI_AVEncMPVGOPSize, Val::U32(0)),
        ];
        match &codec_api {
            Some(api) => {
                for (name, key, val) in &wanted {
                    let mut status = "confirmed";
                    // SAFETY: valid ICodecAPI; GUID pointer valid for the call.
                    if unsafe { api.IsSupported(key) }.is_err() {
                        status = "unsupported";
                    } else {
                        let v = variant(*val);
                        // SAFETY: as above; VT_UI4/VT_BOOL variants own no memory.
                        if let Err(e) = unsafe { api.SetValue(key, &v) } {
                            status = "rejected";
                            setup.notes.push(format!("SetValue {name}: {e}"));
                        }
                    }
                    setup.settings.push(CodecSetting {
                        name: (*name).to_string(),
                        requested: val.text(),
                        status: status.into(),
                        readback: None,
                    });
                }
            }
            None => setup.notes.push("no ICodecAPI".into()),
        }

        // Output type first, then input type (the H.264 encoder MFT's rule).
        // SAFETY: creates a media type; every Set* takes valid GUID pointers and plain values.
        let out_type = unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, p.bitrate)?;
            t.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(p.width) << 32) | u64::from(p.height),
            )?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(p.fps) << 32) | 1)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            t
        };
        // SAFETY: valid MFT and media type.
        unsafe { transform.SetOutputType(output_id, &out_type, 0)? };

        let mut input_set = false;
        for i in 0..32 {
            // SAFETY: valid MFT; MF_E_NO_MORE_TYPES ends the list.
            let Ok(t) = (unsafe { transform.GetInputAvailableType(input_id, i) }) else {
                break;
            };
            // SAFETY: valid media type.
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok() != Some(MFVideoFormat_NV12) {
                continue;
            }
            // SAFETY: valid media type and MFT; plain values.
            unsafe {
                t.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(p.width) << 32) | u64::from(p.height),
                )?;
                t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(p.fps) << 32) | 1)?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                transform.SetInputType(input_id, &t, 0)?;
            }
            input_set = true;
            break;
        }
        if !input_set {
            return Err(Error::new(E_FAIL, "encoder offers no NV12 input type"));
        }

        if let Some(api) = &codec_api {
            for (s, (_, key, val)) in setup.settings.iter_mut().zip(&wanted) {
                if s.status != "confirmed" {
                    continue;
                }
                // SAFETY: valid ICodecAPI; the returned VARIANT is cleared after reading.
                match unsafe { api.GetValue(key) } {
                    Ok(mut v) => {
                        s.readback = read_variant(&v);
                        // SAFETY: a VARIANT returned to us; clearing frees nothing for integer
                        // types and is required for any other.
                        let _ = unsafe { VariantClear(&mut v) };
                        if s.readback.as_deref() != Some(val.text().as_str()) {
                            s.status = "differs".into();
                        }
                    }
                    Err(_) => s.status = "unreadable".into(),
                }
            }
        }
        setup.low_latency_confirmed = setup
            .settings
            .iter()
            .any(|s| s.name == "AVLowLatencyMode" && s.status == "confirmed");

        // SAFETY: valid MFT.
        let info = unsafe { transform.GetOutputStreamInfo(output_id)? };
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        setup.provides_output_samples = provides_samples;
        // SAFETY: valid MFT; streaming messages carry no pointers.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        guard.disarm();
        Ok(Self {
            activate,
            transform,
            codec_api,
            events,
            input_id,
            output_id,
            provides_samples,
            output_buffer_size: info.cbSize.max(p.width * p.height * 3 / 2),
            setup,
            closing: Arc::default(),
            _manager: manager,
        })
    }

    /// Wraps `texture` (NV12, on the encoder's device) in a sample.
    pub fn sample(
        texture: &ID3D11Texture2D,
        time_100ns: i64,
        duration_100ns: i64,
    ) -> Result<Sample> {
        // SAFETY: valid texture on the device the MFT was given; the buffer holds a reference.
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)? };
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            // SAFETY: valid buffers.
            unsafe {
                let len = b2.GetContiguousLength()?;
                buffer.SetCurrentLength(len)?;
            }
        }
        // SAFETY: creates a sample and attaches the buffer; plain time values.
        let sample = unsafe {
            let s = MFCreateSample()?;
            s.AddBuffer(&buffer)?;
            s.SetSampleTime(time_100ns)?;
            s.SetSampleDuration(duration_100ns)?;
            s
        };
        Ok(Sample(sample))
    }

    /// The SPS and PPS the current output type carries (`MF_MT_MPEG_SEQUENCE_HEADER`, Annex-B),
    /// for an encoder that does not repeat them in its keyframes.
    pub fn sequence_header(&self) -> Option<Vec<u8>> {
        // SAFETY: valid MFT; the blob is copied into a buffer of the size MF reports, and only the
        // bytes it wrote are kept.
        unsafe {
            let t = self.transform.GetOutputCurrentType(self.output_id).ok()?;
            let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut buf = vec![0u8; size as usize];
            let mut written = 0u32;
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut written))
                .ok()?;
            buf.truncate(written as usize);
            (!buf.is_empty()).then_some(buf)
        }
    }

    /// Whether `shutdown` has begun: MFT calls that fail now fail because of it.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Asks for an IDR on the next input.
    pub fn force_keyframe(&self) -> Result<()> {
        let api = self.codec_api.as_ref().ok_or_else(Error::empty)?;
        let v = variant(Val::U32(1));
        // SAFETY: valid ICodecAPI; VT_UI4 owns no memory.
        unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v) }
    }

    pub fn set_bitrate(&self, bitrate: u32) -> Result<()> {
        let api = self.codec_api.as_ref().ok_or_else(Error::empty)?;
        let v = variant(Val::U32(bitrate));
        // SAFETY: valid ICodecAPI; VT_UI4 owns no memory.
        unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) }
    }

    /// `ProcessInput`; call only with a `METransformNeedInput` credit.
    pub fn process_input(&self, sample: &Sample) -> Result<()> {
        // SAFETY: valid MFT and sample; the caller holds a NeedInput credit.
        unsafe { self.transform.ProcessInput(self.input_id, &sample.0, 0) }
    }

    /// Discards every pending input and output (`MFT_MESSAGE_COMMAND_FLUSH`).
    pub fn flush(&self) -> Result<()> {
        // SAFETY: valid MFT; the message carries no pointer.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
    }

    /// End of stream, then drain: remaining outputs and `METransformDrainComplete` follow.
    pub fn drain(&self) -> Result<()> {
        // SAFETY: valid MFT; the messages carry no pointer.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
        }
    }

    /// The handle the event thread uses.
    pub fn event_pump(&self) -> EventPump {
        EventPump {
            transform: self.transform.clone(),
            events: self.events.clone(),
            output_id: self.output_id,
            provides_samples: self.provides_samples,
            output_buffer_size: self.output_buffer_size,
            closing: self.closing.clone(),
        }
    }

    /// Shuts the MFT down: a blocked `GetEvent` on the event thread returns `MF_E_SHUTDOWN`.
    /// Idempotent. It can block (the driver releases its session): never call it on a stream
    /// executor.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        // SAFETY: valid MFT; the message carries no pointer. Errors are irrelevant at shutdown.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        if let Ok(s) = self.transform.cast::<IMFShutdown>() {
            // SAFETY: valid interface.
            let _ = unsafe { s.Shutdown() };
        }
        // SAFETY: valid activation object; releases the MFT's resources.
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

/// What the event thread reports.
#[derive(Debug)]
pub enum MfEvent {
    NeedInput,
    Output(Output),
    DrainComplete,
    /// `ProcessOutput` produced nothing (a format change was adopted, or no sample).
    NoOutput,
    /// A failed event status, a failed `ProcessOutput`, or a `GetEvent` that kept failing: the
    /// session cannot go on.
    Failed(Failure),
    Other(u32),
    Shutdown,
}

/// Why a session failed.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    /// The GPU device was removed, reset or hung.
    pub device_lost: bool,
}

impl Failure {
    pub fn new(code: HRESULT, message: String) -> Self {
        Self {
            message,
            device_lost: is_device_lost(code),
        }
    }
}

/// Consecutive `GetEvent` failures before the pump gives up, and the pause between them, so a
/// failing generator cannot spin the event thread.
const GET_EVENT_ATTEMPTS: u32 = 5;
const GET_EVENT_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// One access unit, Annex-B as the MFT wrote it.
#[derive(Debug)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub time_100ns: i64,
    /// `MFSampleExtension_CleanPoint`.
    pub keyframe: bool,
    /// `ProcessOutput` returned (H4).
    pub h4_ns: u64,
}

/// The MFT's event generator and output path, moved to the event thread.
pub struct EventPump {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    output_id: u32,
    provides_samples: bool,
    output_buffer_size: u32,
    closing: Arc<AtomicBool>,
}

// SAFETY: asynchronous MFTs are free-threaded (see `Encoder`), and
// IMFMediaEventGenerator::GetEvent is designed to block on a worker thread.
unsafe impl Send for EventPump {}

impl EventPump {
    /// Blocks until the next event (never call on the executor).
    pub fn next(&self) -> MfEvent {
        let closing = || self.closing.load(Ordering::Acquire);
        let mut attempts = 0;
        let event = loop {
            // SAFETY: valid generator; flags 0 = blocking.
            match unsafe {
                self.events
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            } {
                Ok(e) => break e,
                Err(e) if e.code() == MF_E_SHUTDOWN || closing() => return MfEvent::Shutdown,
                Err(e) => {
                    attempts += 1;
                    if attempts >= GET_EVENT_ATTEMPTS {
                        return MfEvent::Failed(Failure::new(
                            e.code(),
                            format!("GetEvent failed {attempts} times: {e}"),
                        ));
                    }
                    std::thread::sleep(GET_EVENT_RETRY);
                }
            }
        };
        // SAFETY: valid event.
        let ty = unsafe { event.GetType() }.unwrap_or(0);
        // SAFETY: valid event.
        let status: HRESULT = unsafe { event.GetStatus() }.unwrap_or(HRESULT(0));
        if status.is_err() {
            if closing() {
                return MfEvent::Shutdown;
            }
            return MfEvent::Failed(Failure::new(status, format!("event {ty} status {status}")));
        }
        let ty = MF_EVENT_TYPE(ty as i32);
        if ty == METransformNeedInput {
            MfEvent::NeedInput
        } else if ty == METransformHaveOutput {
            match self.process_output() {
                Ok(Some(o)) => MfEvent::Output(o),
                Ok(None) => MfEvent::NoOutput,
                // A HaveOutput queued before END_STREAMING fails once the MFT is shut down
                // (E_UNEXPECTED): the stream is closing, so it is not reported as an error.
                Err(_) if closing() => MfEvent::Shutdown,
                Err(e) => MfEvent::Failed(Failure::new(e.code(), format!("ProcessOutput: {e}"))),
            }
        } else if ty == METransformDrainComplete {
            MfEvent::DrainComplete
        } else {
            MfEvent::Other(ty.0 as u32)
        }
    }

    /// `ProcessOutput` for a `METransformHaveOutput`. On `MF_E_TRANSFORM_STREAM_CHANGE` the new
    /// output type is adopted and `ProcessOutput` called again, as MF's contract asks. Quick Sync
    /// answers that retry with E_UNEXPECTED (or "need more input") and sends the pending frame
    /// with its next `METransformHaveOutput`: both mean "wait for the event", not an error.
    fn process_output(&self) -> Result<Option<Output>> {
        match self.process_output_once()? {
            Step::Output(o) => Ok(Some(o)),
            Step::Nothing => Ok(None),
            Step::StreamChange => match self.process_output_once() {
                Ok(Step::Output(o)) => Ok(Some(o)),
                Ok(Step::Nothing | Step::StreamChange) => Ok(None),
                Err(e) if e.code() == E_UNEXPECTED => Ok(None),
                Err(e) => Err(e),
            },
        }
    }

    fn process_output_once(&self) -> Result<Step> {
        let sample = if self.provides_samples {
            None
        } else {
            // SAFETY: creates a sample with a memory buffer of the size the MFT asked for.
            unsafe {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.output_buffer_size)?)?;
                Some(s)
            }
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: valid MFT; `buffers` is a valid one-element array the MFT fills.
        let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let h4_ns = now_ns();
        let [b] = buffers;
        // Both fields hold owned references now (ours, or the MFT's); they drop here.
        let sample = ManuallyDrop::into_inner(b.pSample);
        drop(ManuallyDrop::into_inner(b.pEvents));
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // SAFETY: valid MFT; adopt the encoder's new output type.
                unsafe {
                    let t = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &t, 0)?;
                }
                return Ok(Step::StreamChange);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Step::Nothing),
            Err(e) => {
                return Err(Error::new(
                    e.code(),
                    format!(
                        "{} (status {status:#x}, provides samples {})",
                        e.message(),
                        self.provides_samples
                    ),
                ));
            }
        }
        let Some(sample) = sample else {
            return Ok(Step::Nothing);
        };
        // SAFETY: valid sample; the contiguous buffer is locked only while copied.
        let (bytes, time_100ns, keyframe) = unsafe {
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            let time = sample.GetSampleTime().unwrap_or(-1);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            (bytes, time, key)
        };
        Ok(Step::Output(Output {
            bytes,
            time_100ns,
            keyframe,
            h4_ns,
        }))
    }
}

/// What one `ProcessOutput` call gave.
enum Step {
    Output(Output),
    Nothing,
    StreamChange,
}

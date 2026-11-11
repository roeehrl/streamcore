//! BGRA -> NV12 at the target size with `ID3D11VideoProcessor` (design doc 4.2): scale and colour
//! conversion in one pass with explicit colour spaces. The work is recorded on the immediate
//! context and returns at once; the caller signals the stream's fence after it.

use super::d3d::Gpu;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_YCbCrA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{Error, Interface, Result};

/// Where the source lands in the output (letterboxed, even-aligned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestRect {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

/// Aspect-preserving fit of `src` into `dst`, centred, with even offsets and sizes.
pub fn letterbox(src: (u32, u32), dst: (u32, u32)) -> DestRect {
    let scale = (f64::from(dst.0) / f64::from(src.0)).min(f64::from(dst.1) / f64::from(src.1));
    let w = ((f64::from(src.0) * scale).round() as u32 & !1).clamp(2, dst.0);
    let h = ((f64::from(src.1) * scale).round() as u32 & !1).clamp(2, dst.1);
    DestRect {
        left: ((dst.0 - w) / 2) & !1,
        top: ((dst.1 - h) / 2) & !1,
        width: w,
        height: h,
    }
}

pub struct VideoProcessor {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pub input_size: (u32, u32),
    pub output_size: (u32, u32),
    pub dest: DestRect,
    inputs: Vec<(usize, ID3D11VideoProcessorInputView)>,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    /// Set when the capture textures cannot be bound as video-processor input: frames are first
    /// copied into this texture (one extra GPU copy, reported).
    copy_in: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    pub copies: u64,
    output_colour: Option<DXGI_COLOR_SPACE_TYPE>,
    /// The cropped part of the input (`None`: all of it).
    source: Option<DestRect>,
}

impl VideoProcessor {
    /// Whether the adapter's video processor takes BGRA in and gives NV12 out.
    pub fn supports_bgra_to_nv12(gpu: &Gpu) -> Result<bool> {
        Self::new(gpu, (1280, 720), (1280, 720), 60)
            .map(|_| true)
            .or_else(|e| {
                if e.code() == windows::Win32::Foundation::E_NOTIMPL {
                    Ok(false)
                } else {
                    Err(e)
                }
            })
    }

    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext1 = gpu.context.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: valid video device; `desc` lives across the call.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc)? };
        // SAFETY: valid enumerator; plain format arguments.
        let (nv12, bgra) = unsafe {
            (
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?,
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?,
            )
        };
        if nv12 & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
            || bgra & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
        {
            return Err(Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                "video processor lacks BGRA in or NV12 out",
            ));
        }
        // SAFETY: valid device and enumerator; rate conversion index 0 always exists.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };
        let dest = letterbox(input, output);
        let src_rect = RECT {
            left: 0,
            top: 0,
            right: input.0 as i32,
            bottom: input.1 as i32,
        };
        let dst_rect = RECT {
            left: dest.left as i32,
            top: dest.top as i32,
            right: (dest.left + dest.width) as i32,
            bottom: (dest.top + dest.height) as i32,
        };
        let target = RECT {
            left: 0,
            top: 0,
            right: output.0 as i32,
            bottom: output.1 as i32,
        };
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
                    Y: 0.0625,
                    Cb: 0.5,
                    Cr: 0.5,
                    A: 1.0,
                },
            },
        };
        // SAFETY: valid context and processor; rect and colour arguments live across each call.
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);
        }
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            input_size: input,
            output_size: output,
            dest,
            inputs: Vec::new(),
            outputs: Vec::new(),
            copy_in: None,
            copies: 0,
            output_colour: None,
            source: None,
        })
    }

    /// Converts only `source` of the input (`None`: all of it), letterboxed into the output.
    /// `source` must lie inside the input with an even origin and size.
    pub fn set_source(&mut self, source: Option<DestRect>) {
        if self.source == source {
            return;
        }
        let src = source.unwrap_or(DestRect {
            left: 0,
            top: 0,
            width: self.input_size.0,
            height: self.input_size.1,
        });
        let dest = letterbox((src.width, src.height), self.output_size);
        let src_rect = RECT {
            left: src.left as i32,
            top: src.top as i32,
            right: (src.left + src.width) as i32,
            bottom: (src.top + src.height) as i32,
        };
        let dst_rect = RECT {
            left: dest.left as i32,
            top: dest.top as i32,
            right: (dest.left + dest.width) as i32,
            bottom: (dest.top + dest.height) as i32,
        };
        // SAFETY: valid context and processor; the rects live across each call.
        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&src_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&dst_rect),
            );
        }
        self.dest = dest;
        self.source = source;
    }

    /// Sets the NV12 output's colour space: BT.709 or BT.601 matrix, limited or full range.
    pub fn set_output_colour(&mut self, bt601: bool, full_range: bool) {
        let cs: DXGI_COLOR_SPACE_TYPE = match (bt601, full_range) {
            (false, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            (false, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
            (true, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            (true, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
        };
        if self.output_colour == Some(cs) {
            return;
        }
        // SAFETY: valid context and processor; plain enum argument.
        unsafe {
            self.video_context
                .VideoProcessorSetOutputColorSpace1(&self.processor, cs)
        };
        self.output_colour = Some(cs);
    }

    /// Registers an NV12 output texture; returns its index for `convert`.
    pub fn add_output(&mut self, tex: &ID3D11Texture2D) -> Result<usize> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: valid device, texture and enumerator; desc and out pointer are valid locals.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                tex,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )?
        };
        self.outputs.push(view.ok_or_else(Error::empty)?);
        Ok(self.outputs.len() - 1)
    }

    fn input_view(&self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        // SAFETY: valid device, texture and enumerator; desc and out pointer are valid locals.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                tex,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )?
        };
        view.ok_or_else(Error::empty)
    }

    fn copy_in_texture(
        &mut self,
        gpu: &Gpu,
    ) -> Result<(ID3D11Texture2D, ID3D11VideoProcessorInputView)> {
        if let Some(c) = &self.copy_in {
            return Ok(c.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.input_size.0,
            Height: self.input_size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: valid device, descriptor and out pointer.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex: ID3D11Texture2D = tex.ok_or_else(Error::empty)?;
        let view = self.input_view(&tex)?;
        self.copy_in = Some((tex.clone(), view.clone()));
        Ok((tex, view))
    }

    /// Whether frames are copied before conversion.
    pub fn copies_input(&self) -> bool {
        self.copy_in.is_some()
    }

    /// Records the conversion of `src` (BGRA, input size) into output `index`. Input views are
    /// cached per texture (a view holds a reference to its texture, so a cached address is never
    /// reused); a texture that cannot be bound as video-processor input is first copied.
    pub fn convert(&mut self, gpu: &Gpu, src: &ID3D11Texture2D, index: usize) -> Result<()> {
        let key = src.as_raw() as usize;
        let view = if self.copy_in.is_some() {
            None
        } else if let Some((_, v)) = self.inputs.iter().find(|(k, _)| *k == key) {
            Some(v.clone())
        } else {
            match self.input_view(src) {
                Ok(v) => {
                    if self.inputs.len() >= 8 {
                        self.inputs.clear();
                    }
                    self.inputs.push((key, v.clone()));
                    Some(v)
                }
                Err(_) => None,
            }
        };
        let view = match view {
            Some(v) => v,
            None => {
                let (tex, v) = self.copy_in_texture(gpu)?;
                // SAFETY: both textures are BGRA of the input size on this device.
                unsafe { gpu.context.CopyResource(&tex, src) };
                self.copies += 1;
                v
            }
        };
        let output = self.outputs.get(index).ok_or_else(Error::empty)?;
        let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ..D3D11_VIDEO_PROCESSOR_STREAM::default()
        }];
        // SAFETY: valid context, processor and output view; `streams` holds a counted reference
        // to the input view for the duration of the call.
        let r = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, output, 0, &streams)
        };
        let [stream] = streams;
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        r
    }
}

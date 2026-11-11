//! The native slot table behind `SurfaceId`s (design doc 3.2), shared by the capture backend, the
//! converter and the encoder of one device: a capture surface keeps its WGC frame until released,
//! a converted surface names its pool slot and the fence value that completes it.

use crate::slots::SlotTable;
use std::sync::{Mutex, MutexGuard};
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub enum Entry {
    /// A WGC frame, held (so WGC cannot reuse its buffer) until `release`.
    Capture {
        texture: ID3D11Texture2D,
        frame: Direct3D11CaptureFrame,
    },
    /// An NV12 pool slot of the converter, complete when the device fence reaches `fence`.
    Converted {
        texture: ID3D11Texture2D,
        slot: usize,
        fence: u64,
    },
}

#[derive(Default)]
pub struct Surfaces {
    table: Mutex<SlotTable<Entry>>,
}

impl Surfaces {
    fn lock(&self) -> MutexGuard<'_, SlotTable<Entry>> {
        self.table.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert(&self, entry: Entry) -> u64 {
        self.lock().insert(entry)
    }

    /// A capture surface's texture.
    pub fn capture_texture(&self, id: u64) -> Option<ID3D11Texture2D> {
        match self.lock().get(id)? {
            Entry::Capture { texture, .. } => Some(texture.clone()),
            Entry::Converted { .. } => None,
        }
    }

    /// A converted surface's texture and fence value.
    pub fn converted(&self, id: u64) -> Option<(ID3D11Texture2D, u64)> {
        match self.lock().get(id)? {
            Entry::Converted { texture, fence, .. } => Some((texture.clone(), *fence)),
            Entry::Capture { .. } => None,
        }
    }

    /// Removes an entry; the caller drops it (closing a WGC frame) after the lock is released.
    pub fn remove(&self, id: u64) -> Option<Entry> {
        self.lock().remove(id)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Entry::Capture { frame, .. } = self {
            // Hand the buffer back to the frame pool now rather than at the last COM release.
            let _ = frame.Close();
        }
    }
}

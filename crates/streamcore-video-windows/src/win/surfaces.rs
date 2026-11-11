//! The native slot table behind `SurfaceId`s (design doc 3.2), shared by the capture backend, the
//! converter and the encoder of one device: a capture surface keeps its WGC frame until released
//! and until the last convert that reads it has completed on the GPU (the converter's contract:
//! Rust's hold ends when `convert` returns), a converted surface names its pool slot and the fence
//! value that completes it.

use crate::slots::SlotTable;
use std::sync::{Mutex, MutexGuard};
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub enum Entry {
    /// A WGC frame, held (so WGC cannot reuse its buffer) until `release` and until the
    /// converter's fence reaches `read_until` (0: no convert has read it). `content` is the
    /// image's size within the texture: smaller when the source shrank and the pool still has
    /// the old size, so only the top-left part of the texture holds the image.
    Capture {
        texture: ID3D11Texture2D,
        frame: Direct3D11CaptureFrame,
        read_until: u64,
        content: (u32, u32),
    },
    /// An NV12 pool slot of the converter, complete when the device fence reaches `fence`.
    Converted {
        texture: ID3D11Texture2D,
        slot: usize,
        fence: u64,
    },
}

/// Released capture entries whose frames a recorded convert still reads.
#[derive(Default)]
struct Deferred {
    /// The converter fence's last completed value seen by the fence thread.
    completed: u64,
    entries: Vec<Entry>,
}

#[derive(Default)]
pub struct Surfaces {
    table: Mutex<SlotTable<Entry>>,
    deferred: Mutex<Deferred>,
}

impl Surfaces {
    fn lock(&self) -> MutexGuard<'_, SlotTable<Entry>> {
        self.table.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert(&self, entry: Entry) -> u64 {
        self.lock().insert(entry)
    }

    /// A capture surface's texture and the size of the image in it.
    pub fn capture_texture(&self, id: u64) -> Option<(ID3D11Texture2D, (u32, u32))> {
        match self.lock().get(id)? {
            Entry::Capture {
                texture, content, ..
            } => Some((texture.clone(), *content)),
            Entry::Converted { .. } => None,
        }
    }

    /// Records that GPU work completing at converter fence `value` reads capture surface `id`.
    pub fn mark_read(&self, id: u64, value: u64) {
        if let Some(Entry::Capture { read_until, .. }) = self.lock().get_mut(id) {
            *read_until = (*read_until).max(value);
        }
    }

    /// Takes a removed entry: returns it to drop now (closing its frame) unless a convert still
    /// reads it, in which case it is kept until `reap` passes its fence value.
    pub fn retire(&self, entry: Entry) -> Option<Entry> {
        let mut deferred = self.deferred.lock().unwrap_or_else(|p| p.into_inner());
        match &entry {
            Entry::Capture { read_until, .. } if *read_until > deferred.completed => {
                deferred.entries.push(entry);
                None
            }
            _ => Some(entry),
        }
    }

    /// The converter fence has completed `value` (fence thread, which passes only what the GPU
    /// has reached): frees the entries it was holding. Entries a convert still reads stay until
    /// a later `reap` or, at the latest, until the table is dropped.
    pub fn reap(&self, value: u64) {
        let done: Vec<Entry> = {
            let mut deferred = self.deferred.lock().unwrap_or_else(|p| p.into_inner());
            deferred.completed = deferred.completed.max(value);
            let completed = deferred.completed;
            let (done, keep) = std::mem::take(&mut deferred.entries).into_iter().partition(
                |e| matches!(e, Entry::Capture { read_until, .. } if *read_until <= completed),
            );
            deferred.entries = keep;
            done
        };
        // Frames close here, after the lock is released.
        drop(done);
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

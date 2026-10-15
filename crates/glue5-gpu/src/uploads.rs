//! Data from the engine to the GPU: written into pooled staging memory and
//! copied by an encoder of its own, submitted just before the commands
//! recorded meanwhile (what `Queue::write_buffer` does, without creating a
//! staging buffer per call: thousands of uploads per frame).
//!
//! Staging chunks travel with the batch that copies from them and come back
//! mapped once it is submitted (see [`recycle`]): batches are submitted by
//! another thread, so nothing here waits for a submit.

use std::sync::{Arc, Mutex};

use crate::byte_cache::ByteCache;

const CHUNK: u64 = 4 << 20;
/// Chunks kept for reuse, at most: loading needs many at once, play few,
/// and memory is scarce on phones.
const KEEP: usize = 16;
const CACHE_BYTES: u64 = if cfg!(any(target_os = "ios", target_os = "android")) { 32 << 20 } else { 64 << 20 };
const CACHE_BUFFER_BYTES: u64 = if cfg!(any(target_os = "ios", target_os = "android")) { 8 << 20 } else { 16 << 20 };
/// Submit large streaming bursts incrementally instead of retaining all their
/// staging chunks in one command batch. A single larger upload is still valid.
pub const BATCH_BYTES: u64 = 32 << 20;

/// Mapped chunks, ready to be written.
pub type FreeChunks = Arc<Mutex<ByteCache<wgpu::Buffer>>>;

pub struct Uploads {
    device: wgpu::Device,
    encoder: Option<wgpu::CommandEncoder>,
    /// The chunk being filled and its next free byte.
    current: Option<(wgpu::Buffer, u64)>,
    /// Chunks written since the last batch.
    used: Vec<wgpu::Buffer>,
    free: FreeChunks,
    pub bytes: u64,
    batch_bytes: u64,
}

impl Uploads {
    pub fn new(device: wgpu::Device) -> Uploads {
        Uploads {
            device,
            encoder: None,
            current: None,
            used: Vec::new(),
            free: Arc::new(Mutex::new(ByteCache::new(CACHE_BYTES, CACHE_BUFFER_BYTES, KEEP))),
            bytes: 0,
            batch_bytes: 0,
        }
    }

    pub fn free_chunks(&self) -> FreeChunks {
        self.free.clone()
    }

    pub fn cached_bytes(&self) -> u64 {
        self.free.lock().unwrap().bytes()
    }

    pub fn batch_full(&self) -> bool {
        self.batch_bytes >= BATCH_BYTES
    }

    fn encoder(&mut self) -> &mut wgpu::CommandEncoder {
        self.encoder.get_or_insert_with(|| {
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("uploads") })
        })
    }

    /// `size` bytes of staging memory, `align`-aligned: (chunk, offset).
    fn allocate(&mut self, size: u64, align: u64) -> (wgpu::Buffer, u64) {
        // Mapped ranges start at multiples of MAP_ALIGNMENT.
        let align = align.max(wgpu::MAP_ALIGNMENT);
        if let Some((buffer, next)) = &mut self.current {
            let at = next.next_multiple_of(align);
            if at + size <= buffer.size() {
                *next = at + size;
                return (buffer.clone(), at);
            }
        }
        if let Some((full, _)) = self.current.take() {
            self.used.push(full);
        }
        let reused = self.free.lock().unwrap().take(size.max(CHUNK), false);
        let buffer = reused.unwrap_or_else(|| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("upload staging"),
                size: size.max(CHUNK).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT),
                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            })
        });
        self.current = Some((buffer.clone(), size));
        (buffer, 0)
    }

    /// `data` into `target` at `offset`; both 4-byte aligned in length and offset
    /// (a shorter tail is padded with zeros).
    pub fn buffer(&mut self, target: &wgpu::Buffer, offset: u64, data: &[u8]) {
        let size = (data.len() as u64).next_multiple_of(4);
        if size == 0 {
            return;
        }
        self.bytes += size;
        self.batch_bytes += size;
        let (chunk, at) = self.allocate(size, wgpu::COPY_BUFFER_ALIGNMENT);
        {
            let mut view = chunk.slice(at..at + size).get_mapped_range_mut().expect("staging memory is mapped");
            view.slice(..data.len()).copy_from_slice(data);
            view.slice(data.len()..).fill(0);
        }
        self.encoder().copy_buffer_to_buffer(&chunk, at, target, offset, size);
    }

    /// Rows of texel blocks into a texture region: `rows` rows of `row_bytes`
    /// per layer, `bpr` bytes apart in `data` and `src_rows` rows per layer.
    #[allow(clippy::too_many_arguments)]
    pub fn texture(
        &mut self,
        target: wgpu::TexelCopyTextureInfo,
        data: &[u8],
        bpr: u32,
        src_rows: u32,
        rows: u32,
        extent: wgpu::Extent3d,
        row_bytes: u32,
    ) {
        let pitch = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let layers = extent.depth_or_array_layers;
        let size = pitch as u64 * rows as u64 * layers as u64;
        if size == 0 {
            return;
        }
        self.bytes += size;
        self.batch_bytes += size;
        let (chunk, base) = self.allocate(size, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64);
        {
            let mut view = chunk.slice(base..base + size).get_mapped_range_mut().expect("staging memory is mapped");
            for layer in 0..layers {
                for row in 0..rows {
                    let src = ((layer * src_rows + row) * bpr) as usize;
                    let dst = ((layer * rows + row) * pitch) as usize;
                    let n = (row_bytes as usize).min(data.len().saturating_sub(src));
                    view.slice(dst..dst + n).copy_from_slice(&data[src..src + n]);
                }
            }
        }
        self.encoder().copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &chunk,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: base,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(rows),
                },
            },
            target,
            extent,
        );
    }

    /// The uploads to submit before the next commands, and the chunks they
    /// copy from (unmapped; to [`recycle`] after the submit).
    pub fn finish(&mut self) -> (Option<wgpu::CommandEncoder>, Vec<wgpu::Buffer>) {
        self.batch_bytes = 0;
        if let Some((chunk, _)) = self.current.take() {
            self.used.push(chunk);
        }
        let chunks = std::mem::take(&mut self.used);
        for chunk in &chunks {
            chunk.unmap();
        }
        (self.encoder.take(), chunks)
    }
}

/// Maps chunks again once the GPU has copied from them, back into `free`.
pub fn recycle(chunks: Vec<wgpu::Buffer>, free: &FreeChunks) {
    for chunk in chunks {
        let (back, free) = (chunk.clone(), free.clone());
        chunk.slice(..).map_async(wgpu::MapMode::Write, move |r| {
            let kept = if r.is_ok() { free.lock().unwrap().insert(back.size(), back) } else { Err(back) };
            if let Err(back) = kept {
                back.destroy();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a Metal or Vulkan device"]
    fn upload_bursts_submit_in_order_and_cache_is_byte_bounded() {
        let gpu = crate::Device::open(&crate::instance(), None).unwrap();
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut uploads = Uploads::new(gpu.device.clone());
        // A small test budget exercises the same policy without relying on
        // the platform on which this test is run.
        uploads.free = Arc::new(Mutex::new(ByteCache::new(CHUNK * 2, CHUNK, KEEP)));
        let target = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upload budget test"),
            size: CHUNK,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let mut data = vec![0; CHUNK as usize];
        for i in 0..9 {
            data.fill(i);
            uploads.buffer(&target, 0, &data);
            assert_eq!(uploads.batch_full(), i == 7);
            if uploads.batch_full() || i == 8 {
                let (encoder, chunks) = uploads.finish();
                let index = gpu.queue.submit(encoder.map(|e| e.finish()));
                recycle(chunks, &uploads.free);
                gpu.device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index),
                        timeout: Some(std::time::Duration::from_secs(10)),
                    })
                    .unwrap();
                assert!(uploads.cached_bytes() <= CHUNK * 2);
                assert!(!uploads.batch_full());
            }
        }
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: CHUNK,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&target, 0, &readback, 0, CHUNK);
        let index = gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .unwrap();
        rx.recv().unwrap().unwrap();
        {
            let view = readback.slice(..).get_mapped_range().unwrap();
            assert!(view.iter().all(|b| *b == 8), "latest upload wins across budget submits");
        }
        readback.unmap();
        assert!(pollster::block_on(scope.pop()).is_none());
    }
}

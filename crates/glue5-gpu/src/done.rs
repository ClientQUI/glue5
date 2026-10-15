//! Completions: what the engine waits on (fences, queries, readbacks) is
//! finished by a thread of its own once the GPU is done with the submission,
//! so the command thread never blocks on the GPU for them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glue5_runtime::gpu::{OCCLUSION_VISIBLE_COUNT, store_query};
use glue5_runtime::memory::Mem;

use crate::byte_cache::ByteCache;

pub enum Work {
    /// A fence: store `value` at `at` and wake waiters.
    Store {
        at: u64,
        value: u32,
    },
    Query {
        at: u64,
        result: u64,
        seq: u32,
    },
    /// GPU data into wasm memory, then the flag word to 0.
    Readback {
        staging: Staging,
        copy: Copy,
        flag: u64,
    },
    /// Occlusion results: per query, the sum of its slots' 8-byte counts.
    Occlusion {
        staging: Staging,
        queries: Vec<OcclusionResult>,
    },
    /// Pass timestamps (`GLUE5_GPU_PROFILE`).
    Profile {
        staging: Staging,
        resolved: crate::profile::Resolved,
    },
    /// A frame's batch timestamps, for the performance overlay.
    #[cfg(not(target_vendor = "apple"))]
    GpuTime {
        staging: Staging,
        pairs: u32,
        period: f32,
    },
    Screenshot {
        staging: Staging,
        width: u32,
        height: u32,
        bpr: u32,
        format: wgpu::TextureFormat,
        path: PathBuf,
    },
}

pub struct OcclusionResult {
    pub at: u64,
    pub seq: u32,
    /// An occlusion predicate: its result is whether any sample passed.
    pub predicate: bool,
    /// Byte offsets of the query's counts in the staging buffer.
    pub offsets: Vec<u32>,
}

pub enum Copy {
    Bytes {
        to: u64,
        len: usize,
    },
    /// Rows of a texture copy (`bpr`-aligned) to the engine's pitches.
    Rows {
        to: u64,
        rows: u32,
        row_bytes: u32,
        bpr: u32,
        row_pitch: u32,
        depth: u32,
        slice_pitch: u32,
        /// Keep only this byte of each 4-byte texel (an A8 texture stored as
        /// RGBA8 reads back as A8).
        pick: Option<u8>,
    },
}

/// A mappable buffer being read back; returned to the pool afterwards.
pub struct Staging {
    pub buffer: wgpu::Buffer,
    mapped: Arc<AtomicU8>,
}

/// MAP_READ buffers, reused.
pub struct StagingPool {
    free: Mutex<ByteCache<wgpu::Buffer>>,
}

impl Default for StagingPool {
    fn default() -> Self {
        let mobile = cfg!(any(target_os = "ios", target_os = "android"));
        Self {
            free: Mutex::new(ByteCache::new(
                if mobile { 16 << 20 } else { 32 << 20 },
                if mobile { 8 << 20 } else { 16 << 20 },
                16,
            )),
        }
    }
}

impl StagingPool {
    pub fn get(&self, device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        let size = size.max(256).next_power_of_two();
        let mut free = self.free.lock().unwrap();
        if let Some(buffer) = free.take(size, true) {
            return buffer;
        }
        drop(free);
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
    }

    fn put(&self, buffer: wgpu::Buffer) {
        let kept = self.free.lock().unwrap().insert(buffer.size(), buffer);
        if let Err(buffer) = kept {
            buffer.destroy();
        }
    }

    pub fn cached_bytes(&self) -> u64 {
        self.free.lock().unwrap().bytes()
    }
}

impl Staging {
    /// Starts mapping `buffer`; call right after the submit that fills it.
    pub fn map(buffer: wgpu::Buffer) -> Staging {
        let mapped = Arc::new(AtomicU8::new(0));
        let m = mapped.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            if let Err(e) = &r {
                log::warn!("gpu: mapping a readback: {e}");
            }
            m.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release)
        });
        Staging { buffer, mapped }
    }
}

pub struct Done {
    tx: Sender<(wgpu::SubmissionIndex, Vec<Work>)>,
}

impl Done {
    pub fn start(device: wgpu::Device, mem: Mem, pool: Arc<StagingPool>) -> Done {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("gpu done".into())
            .spawn(move || run(device, mem, pool, rx))
            .expect("spawning the GPU completion thread");
        Done { tx }
    }

    /// Runs `work` once `submission` has completed.
    pub fn after(&self, submission: wgpu::SubmissionIndex, work: Vec<Work>) {
        if !work.is_empty() {
            let _ = self.tx.send((submission, work));
        }
    }
}

fn run(device: wgpu::Device, mem: Mem, pool: Arc<StagingPool>, rx: Receiver<(wgpu::SubmissionIndex, Vec<Work>)>) {
    // The engine paces its frames on what this thread completes.
    glue5_runtime::qos::bind_host(glue5_runtime::qos::Qos::Interactive);
    while let Ok((submission, works)) = rx.recv() {
        glue5_runtime::qos::refresh();
        glue5_runtime::diagnostics::gpu_completion_stage(1);
        let result = wait_for_submission(
            || {
                device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(submission.clone()),
                        timeout: Some(Duration::from_secs(1)),
                    })
                    .map(|_| ())
            },
            |elapsed| {
                glue5_runtime::diagnostics::fault(format_args!(
                    "gpu completion stalled: submission={submission:?} elapsed={}ms pending_work={}; retaining readbacks and fences",
                    elapsed.as_millis(),
                    works.len()
                ))
            },
        );
        if let Err(e) = result {
            glue5_runtime::exit::fail(format!("GPU submission wait failed: {e}"));
            // Never signal completion for work that did not finish.
            return;
        }
        glue5_runtime::diagnostics::gpu_completion_stage(2);
        for work in works {
            finish(&device, mem, &pool, work);
        }
        glue5_runtime::diagnostics::gpu_completion_stage(0);
    }
}

/// A timeout is not completion: keep the submission and its work alive and
/// retry. Only a successful poll permits writing engine completion flags.
fn wait_for_submission(
    mut poll: impl FnMut() -> Result<(), wgpu::PollError>,
    mut stalled: impl FnMut(Duration),
) -> Result<(), wgpu::PollError> {
    let started = Instant::now();
    let mut reported = Duration::ZERO;
    loop {
        match poll() {
            Ok(()) => return Ok(()),
            Err(wgpu::PollError::Timeout) => {
                let elapsed = started.elapsed();
                if elapsed.saturating_sub(reported) >= Duration::from_secs(5) {
                    reported = elapsed;
                    stalled(elapsed);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_does_not_complete_submission() {
        let mut calls = 0;
        wait_for_submission(
            || {
                calls += 1;
                if calls < 3 { Err(wgpu::PollError::Timeout) } else { Ok(()) }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(calls, 3);
    }

    #[test]
    fn invalid_submission_does_not_retry_or_succeed() {
        let mut calls = 0;
        let result = wait_for_submission(
            || {
                calls += 1;
                Err(wgpu::PollError::WrongSubmissionIndex(2, 1))
            },
            |_| {},
        );
        assert!(matches!(result, Err(wgpu::PollError::WrongSubmissionIndex(2, 1))));
        assert_eq!(calls, 1);
    }
}

/// The mapped contents of a staging buffer, or `None` if mapping failed.
fn mapped(device: &wgpu::Device, s: &Staging) -> Option<wgpu::BufferView> {
    if s.mapped.load(Ordering::Acquire) == 0 {
        // The relevant submission already completed. Pump callbacks without
        // waiting for unrelated, newer submissions.
        let started = Instant::now();
        let mut reported = Duration::ZERO;
        while s.mapped.load(Ordering::Acquire) == 0 {
            if let Err(e) = device.poll(wgpu::PollType::Poll) {
                glue5_runtime::diagnostics::fault(format_args!("gpu mapping poll failed: {e}"));
            }
            if started.elapsed().saturating_sub(reported) >= Duration::from_secs(5) {
                reported = started.elapsed();
                glue5_runtime::diagnostics::fault(format_args!(
                    "gpu mapping callback stalled: elapsed={}ms buffer_bytes={}",
                    reported.as_millis(),
                    s.buffer.size()
                ));
            }
            if s.mapped.load(Ordering::Acquire) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    if s.mapped.load(Ordering::Acquire) == 1 { s.buffer.slice(..).get_mapped_range().ok() } else { None }
}

fn release(pool: &StagingPool, s: Staging) {
    if s.mapped.load(Ordering::Acquire) == 1 {
        s.buffer.unmap();
        pool.put(s.buffer);
    }
}

fn finish(device: &wgpu::Device, mem: Mem, pool: &StagingPool, work: Work) {
    match work {
        Work::Store { at, value } => mem.store_notify(at, value),
        Work::Query { at, result, seq } => store_query(mem, at, result, seq),
        Work::Readback { staging, copy, flag } => {
            if let Some(data) = mapped(device, &staging) {
                match copy {
                    Copy::Bytes { to, len } => mem.bytes_mut(to, len).copy_from_slice(&data[..len]),
                    Copy::Rows { to, rows, row_bytes, bpr, row_pitch, depth, slice_pitch, pick } => {
                        for z in 0..depth {
                            for row in 0..rows {
                                let from = ((z * rows + row) * bpr) as usize;
                                let src = &data[from..from + row_bytes as usize];
                                let dst = to + z as u64 * slice_pitch as u64 + row as u64 * row_pitch as u64;
                                match pick {
                                    None => mem.bytes_mut(dst, src.len()).copy_from_slice(src),
                                    Some(i) => {
                                        let out = mem.bytes_mut(dst, src.len() / 4);
                                        for (o, texel) in out.iter_mut().zip(src.as_chunks::<4>().0) {
                                            *o = texel[i as usize];
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                log::warn!("gpu: a readback could not be mapped");
            }
            release(pool, staging);
            if flag != 0 {
                mem.store_notify(flag, 0);
            }
        }
        Work::Occlusion { staging, queries } => {
            let counts: Option<Vec<u64>> = mapped(device, &staging).map(|data| {
                queries
                    .iter()
                    .map(|q| {
                        q.offsets
                            .iter()
                            .map(|&o| u64::from_le_bytes(data[o as usize..o as usize + 8].try_into().unwrap()))
                            .sum()
                    })
                    .collect()
            });
            release(pool, staging);
            for (i, q) in queries.iter().enumerate() {
                // Samples that passed (counted by the patched wgpu-hal): the
                // engine divides them by an area (the sun's visibility) and
                // compares them with pixel thresholds. A lost readback counts
                // as visible.
                let count = counts.as_ref().map_or(OCCLUSION_VISIBLE_COUNT, |c| c[i]);
                store_query(mem, q.at, if q.predicate { (count != 0) as u64 } else { count }, q.seq);
            }
        }
        Work::Profile { staging, resolved } => {
            if let Some(data) = mapped(device, &staging) {
                resolved.add(&data);
            }
            release(pool, staging);
        }
        #[cfg(not(target_vendor = "apple"))]
        Work::GpuTime { staging, pairs, period } => {
            if let Some(data) = mapped(device, &staging) {
                crate::perf::timestamps_done(&data, pairs, period);
            }
            release(pool, staging);
        }
        Work::Screenshot { staging, width, height, bpr, format, path } => {
            let pixels = mapped(device, &staging).map(|data| data.to_vec());
            release(pool, staging);
            // Encoding takes tens of milliseconds: not on this thread.
            if let Some(pixels) = pixels {
                std::thread::spawn(move || {
                    match crate::present::write_png(&path, &pixels, width, height, bpr, format) {
                        Ok(()) => log::info!("gpu: screenshot {}", path.display()),
                        Err(e) => log::warn!("gpu: screenshot {}: {e:#}", path.display()),
                    }
                });
            }
        }
    }
}

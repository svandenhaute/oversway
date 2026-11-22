//! Output captures with ext-image-copy-capture-v1 into shm buffers. Captures are
//! only requested while the overlay is hidden. A worker thread downscales each full
//! size capture with a box filter; the main thread uploads the result.

use crate::State;
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::shm::raw::RawPool;
use std::time::Instant;
use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::{Connection, QueueHandle, WEnum};
use wayland_protocols::ext::image_copy_capture::v1::client::{ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_session_v1 as session};

/// Largest width of the stored (downscaled) capture. Used for thumbnails and as blur source.
pub const STORED_WIDTH: u32 = 1024;

#[derive(Default)]
pub struct Capture {
    pub source: Option<wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1>,
    pub session: Option<session::ExtImageCopyCaptureSessionV1>,
    pub size: (u32, u32),
    pub format: Option<wl_shm::Format>,
    pub pool: Option<RawPool>,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub frame: Option<frame::ExtImageCopyCaptureFrameV1>,
    /// Workspace visible on the output and the workspace generation when the frame was requested.
    pub frame_workspace: String,
    pub frame_generation: u64,
    pub due: Option<Instant>,
    /// A downscale job is in flight; do not reuse the buffer.
    pub busy: bool,
}

pub struct Job {
    pub output: String,
    pub workspace: String,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub layout: Layout,
    pub pixels: Vec<u8>,
}

pub struct Downscaled {
    pub output: String,
    pub workspace: String,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Bytes per pixel and the byte offsets of red, green and blue in memory.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub bytes: u32,
    pub red: usize,
    pub green: usize,
    pub blue: usize,
}

pub fn layout(format: wl_shm::Format) -> Option<Layout> {
    use wl_shm::Format::*;
    let (bytes, red, green, blue) = match format {
        Argb8888 | Xrgb8888 => (4, 2, 1, 0),
        Abgr8888 | Xbgr8888 => (4, 0, 1, 2),
        Bgra8888 | Bgrx8888 => (4, 1, 2, 3),
        Rgba8888 | Rgbx8888 => (4, 3, 2, 1),
        Bgr888 => (3, 0, 1, 2),
        Rgb888 => (3, 2, 1, 0),
        _ => return None,
    };
    Some(Layout { bytes, red, green, blue })
}

/// Integer box filter to at most STORED_WIDTH wide; output RGBA with opaque alpha.
pub fn downscale(job: Job) -> Downscaled {
    let factor = job.width.div_ceil(STORED_WIDTH).max(1);
    let width = job.width / factor;
    let height = job.height / factor;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let area = factor * factor;
    let Layout { bytes, red, green, blue } = job.layout;
    let bytes = bytes as usize;
    for y in 0..height {
        let mut sums = vec![[0u32; 3]; width as usize];
        for row in 0..factor {
            let line = &job.pixels[((y * factor + row) * job.stride) as usize..];
            for (x, sum) in sums.iter_mut().enumerate() {
                let start = x * factor as usize * bytes;
                for pixel in line[start..start + factor as usize * bytes].chunks_exact(bytes) {
                    sum[0] += pixel[red] as u32;
                    sum[1] += pixel[green] as u32;
                    sum[2] += pixel[blue] as u32;
                }
            }
        }
        for (x, sum) in sums.iter().enumerate() {
            let target = ((y * width) as usize + x) * 4;
            pixels[target] = (sum[0] / area) as u8;
            pixels[target + 1] = (sum[1] / area) as u8;
            pixels[target + 2] = (sum[2] / area) as u8;
            pixels[target + 3] = 255;
        }
    }
    Downscaled { output: job.output, workspace: job.workspace, width, height, pixels }
}

fn cache_directory() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var("XDG_CACHE_HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::Path::new(&home).join(".cache")).join("oversway")
}

fn sanitize(name: &str) -> String {
    name.chars().map(|character| if character.is_alphanumeric() || character == '-' { character } else { '_' }).collect()
}

/// Thumbnails survive restarts: raw RGBA after a small header (width, height, names).
pub fn save_cached(image: &Downscaled) {
    let directory = cache_directory();
    let _ = std::fs::create_dir_all(&directory);
    let header = format!("{} {}\n{}\n{}\n", image.width, image.height, image.output, image.workspace);
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(&image.pixels);
    let path = directory.join(format!("{}--{}.thumbnail", sanitize(&image.output), sanitize(&image.workspace)));
    let _ = std::fs::write(path, bytes);
}

pub fn load_cached() -> Vec<Downscaled> {
    let Ok(reader) = std::fs::read_dir(cache_directory()) else { return Vec::new() };
    reader
        .flatten()
        .filter(|item| item.path().extension().is_some_and(|extension| extension == "thumbnail"))
        .filter_map(|item| {
            let bytes = std::fs::read(item.path()).ok()?;
            let mut lines = bytes.splitn(4, |&byte| byte == b'\n');
            let size = std::str::from_utf8(lines.next()?).ok()?;
            let (width, height) = size.split_once(' ')?;
            let (width, height): (u32, u32) = (width.parse().ok()?, height.parse().ok()?);
            let output = String::from_utf8(lines.next()?.to_vec()).ok()?;
            let workspace = String::from_utf8(lines.next()?.to_vec()).ok()?;
            let pixels = lines.next()?.to_vec();
            (pixels.len() == (width * height * 4) as usize).then_some(Downscaled { output, workspace, width, height, pixels })
        })
        .collect()
}

pub struct SessionData {
    pub output: String,
}
pub struct FrameData {
    pub output: String,
}

fn capture_of<'a>(state: &'a mut State, output: &str) -> Option<&'a mut Capture> {
    state.outputs.iter_mut().find(|view| view.name == output).map(|view| &mut view.capture)
}

impl Dispatch2<session::ExtImageCopyCaptureSessionV1, State> for SessionData {
    fn event(&self, state: &mut State, _: &session::ExtImageCopyCaptureSessionV1, event: session::Event, _: &Connection, _: &QueueHandle<State>) {
        log::debug!("capture session {}: {event:?}", self.output);
        let Some(capture) = capture_of(state, &self.output) else { return };
        match event {
            session::Event::BufferSize { width, height } => {
                if capture.size != (width, height) {
                    capture.size = (width, height);
                    capture.buffer.take().map(|buffer| buffer.destroy());
                }
            }
            session::Event::ShmFormat { format: WEnum::Value(format) } => {
                // Prefer 4-byte formats; accept any layout we can read.
                let rank = |format: Option<wl_shm::Format>| format.and_then(layout).map_or(0, |layout| layout.bytes);
                if rank(Some(format)) > rank(capture.format) {
                    capture.format = Some(format);
                    capture.buffer.take().map(|buffer| buffer.destroy());
                }
            }
            session::Event::Done => {
                if capture.due.is_none() && capture.frame.is_none() {
                    capture.due = Some(Instant::now());
                }
            }
            session::Event::Stopped => {
                log::warn!("capture session for {} stopped", self.output);
                capture.session.take().map(|session| session.destroy());
                capture.frame.take().map(|frame| frame.destroy());
            }
            _ => {}
        }
    }
}

impl Dispatch2<frame::ExtImageCopyCaptureFrameV1, State> for FrameData {
    fn event(&self, state: &mut State, proxy: &frame::ExtImageCopyCaptureFrameV1, event: frame::Event, _: &Connection, _: &QueueHandle<State>) {
        log::debug!("capture frame {}: {event:?}", self.output);
        match event {
            frame::Event::Ready => state.capture_ready(&self.output, proxy),
            frame::Event::Failed { reason } => {
                log::debug!("capture failed on {}: {reason:?}", self.output);
                if let Some(capture) = capture_of(state, &self.output) {
                    if capture.frame.as_ref() == Some(proxy) {
                        capture.frame.take().map(|frame| frame.destroy());
                        capture.due = Some(Instant::now() + std::time::Duration::from_secs(2));
                    }
                }
            }
            _ => {}
        }
    }
}

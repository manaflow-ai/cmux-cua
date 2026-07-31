//! Host-owned native application surfaces.
//!
//! ScreenCaptureKit and CGEvent remain in the independently registered helper
//! process, so cmux itself never acquires Screen Recording or Accessibility
//! authority. Control and input use the authenticated daemon socket. Frames use
//! a permission-restricted, versioned triple-buffer in POSIX shared memory.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::os::fd::RawFd;
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use foreign_types::ForeignType;
use screencapturekit::cm::{CMSampleBufferExt, CMSampleBufferSCExt, FrameInfo, SCFrameStatus};
use screencapturekit::prelude::{
    PixelFormat, SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutputType,
};
use screencapturekit::stream::StreamCallbacks;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{permissions, windows};

const FRAME_HEADER_BYTE_COUNT: usize = 64;
const FRAME_PUBLISHED_WORD_OFFSET: usize = 32;
const FRAME_FAILURE_WORD: u64 = 1 << 63;
const FRAME_SLOT_VERSION_OFFSET: usize = 40;
const FRAME_SLOT_VERSION_STRIDE: usize = 8;
const FRAME_MAGIC: u32 = 0x434D_5846;
const FRAME_VERSION: u32 = 2;
const FRAME_PIXEL_BYTE_COUNT: usize = 4;
const FRAME_SLOT_COUNT: usize = 3;
const FRAME_UNAVAILABLE_SLOT: u64 = FRAME_SLOT_COUNT as u64;
// Retain more than eight seconds at the maximum 120 fps so bounded input
// transport stalls cannot detach a queued event from its displayed frame.
const FRAME_GEOMETRY_HISTORY_COUNT: usize = 1_024;
const SOURCE_SIZE_MATCH_TOLERANCE_POINTS: f64 = 1.0;
const MAXIMUM_FRAME_DIMENSION: usize = 16_384;
const MAXIMUM_FRAME_RING_BYTE_COUNT: usize = 256 * 1_024 * 1_024;
pub const MAXIMUM_APPLICATION_SURFACE_EVENT_BATCH_COUNT: usize = 64;
const BGRA_PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"BGRA");
const APPLICATION_SURFACE_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplicationSurfacePermissionUse {
    WindowListing,
    CaptureAndInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationSurfaceError {
    #[error("accessibility_permission_required")]
    AccessibilityPermissionRequired,
    #[error("screen_recording_permission_required")]
    ScreenRecordingPermissionRequired,
    #[error("application_window_unavailable")]
    WindowUnavailable,
    #[error("application_surface_point_outside_content")]
    PointOutsideContent,
    #[error("application_surface_session_unavailable")]
    SessionUnavailable,
    #[error("application_surface_capture_failed")]
    CaptureFailed,
    #[error("application_surface_capture_unavailable")]
    CaptureUnavailable,
}

impl ApplicationSurfaceError {
    pub const fn protocol_code(self) -> &'static str {
        match self {
            Self::AccessibilityPermissionRequired | Self::ScreenRecordingPermissionRequired => {
                "permission_required"
            }
            Self::WindowUnavailable => "window_unavailable",
            Self::PointOutsideContent => "point_outside_content",
            Self::SessionUnavailable => "session_unavailable",
            Self::CaptureFailed => "capture_failed",
            Self::CaptureUnavailable => "capture_unavailable",
        }
    }
}

pub fn error_protocol_code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<ApplicationSurfaceError>()
        .map(|error| error.protocol_code())
        .unwrap_or("application_surface_failed")
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationWindow {
    pub window_id: u32,
    pub process_id: i32,
    pub owner: String,
    pub title: String,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameTransportDescriptor {
    pub shared_memory_name: String,
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    pub slot_count: usize,
    pub shared_memory_byte_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationSurfaceStartResult {
    pub session_id: String,
    pub frame_transport: FrameTransportDescriptor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSurfaceStartRequest {
    pub window_id: u32,
    pub process_id: i32,
    #[serde(default = "default_frame_rate")]
    pub frame_rate: u32,
}

fn default_frame_rate() -> u32 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSurfaceEvent {
    pub session: String,
    pub kind: String,
    #[serde(default)]
    pub frame_sequence: Option<u64>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    #[serde(default)]
    pub button: String,
    pub key_code: Option<u16>,
    pub key_down: Option<bool>,
    #[serde(default)]
    pub modifiers: u64,
    #[serde(default = "default_click_count")]
    pub click_count: i64,
    pub delta_x: Option<f64>,
    pub delta_y: Option<f64>,
}

impl ApplicationSurfaceEvent {
    fn required_frame_sequence(&self) -> anyhow::Result<u64> {
        self.frame_sequence
            .filter(|sequence| *sequence > 0 && *sequence <= (i64::MAX as u64 >> 2))
            .ok_or_else(|| anyhow!("application-surface frame sequence is required"))
    }

    fn pointer_coordinates(&self) -> anyhow::Result<(f64, f64)> {
        let (Some(x), Some(y)) = (self.x, self.y) else {
            bail!("application-surface pointer coordinates are required");
        };
        if !x.is_finite() || !y.is_finite() {
            bail!("application-surface pointer coordinates must be finite");
        }
        Ok((x, y))
    }

    fn scroll_values(&self) -> anyhow::Result<(f64, f64, f64, f64)> {
        let (Some(x), Some(y), Some(delta_x), Some(delta_y)) =
            (self.x, self.y, self.delta_x, self.delta_y)
        else {
            bail!("application-surface scroll values are required");
        };
        if !x.is_finite() || !y.is_finite() || !delta_x.is_finite() || !delta_y.is_finite() {
            bail!("application-surface scroll values must be finite");
        }
        Ok((x, y, delta_x, delta_y))
    }

    fn key_values(&self) -> anyhow::Result<(u16, bool)> {
        let (Some(key_code), Some(key_down)) = (self.key_code, self.key_down) else {
            bail!("application-surface key values are required");
        };
        Ok((key_code, key_down))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSurfaceEventBatchRequest {
    pub events: Vec<ApplicationSurfaceEvent>,
}

impl ApplicationSurfaceEventBatchRequest {
    pub fn into_validated_events(self) -> Result<Vec<ApplicationSurfaceEvent>, &'static str> {
        validate_event_batch(&self.events)
            .map_err(|_| "Invalid application-surface event batch")?;
        Ok(self.events)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApplicationSurfaceKeyboardTarget {
    window_id: u32,
    process_id: i32,
}

impl ApplicationSurfaceKeyboardTarget {
    fn new(window_id: u32, process_id: i32) -> Option<Self> {
        (window_id > 0 && process_id > 0).then_some(Self {
            window_id,
            process_id,
        })
    }
}

fn default_click_count() -> i64 {
    1
}

#[derive(Debug, Clone, Copy)]
struct FrameLayout {
    width: usize,
    height: usize,
    bytes_per_row: usize,
    slot_byte_count: usize,
    total_byte_count: usize,
}

impl FrameLayout {
    fn new(width: usize, height: usize) -> anyhow::Result<Self> {
        if !(1..=MAXIMUM_FRAME_DIMENSION).contains(&width)
            || !(1..=MAXIMUM_FRAME_DIMENSION).contains(&height)
        {
            bail!("frame dimensions are outside supported bounds");
        }
        let bytes_per_row = width
            .checked_mul(FRAME_PIXEL_BYTE_COUNT)
            .ok_or_else(|| anyhow!("frame row size overflowed"))?;
        let slot_byte_count = bytes_per_row
            .checked_mul(height)
            .ok_or_else(|| anyhow!("frame slot size overflowed"))?;
        let unaligned = FRAME_HEADER_BYTE_COUNT
            .checked_add(
                slot_byte_count
                    .checked_mul(FRAME_SLOT_COUNT)
                    .ok_or_else(|| anyhow!("frame ring size overflowed"))?,
            )
            .ok_or_else(|| anyhow!("frame ring size overflowed"))?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            bail!("page geometry is unavailable");
        }
        let page_size = usize::try_from(page_size).context("invalid page geometry")?;
        let padding = (page_size - (unaligned % page_size)) % page_size;
        let total_byte_count = unaligned
            .checked_add(padding)
            .ok_or_else(|| anyhow!("frame mapping size overflowed"))?;
        if total_byte_count > MAXIMUM_FRAME_RING_BYTE_COUNT {
            bail!("frame mapping exceeds the supported size");
        }
        Ok(Self {
            width,
            height,
            bytes_per_row,
            slot_byte_count,
            total_byte_count,
        })
    }

    fn slot_offset(self, slot: usize) -> Option<usize> {
        (slot < FRAME_SLOT_COUNT).then(|| FRAME_HEADER_BYTE_COUNT + slot * self.slot_byte_count)
    }
}

struct SharedFrameRing {
    name: CString,
    mapping: NonNull<u8>,
    layout: FrameLayout,
    next_sequence: AtomicU64,
    // The host echoes the sequence of its displayed frame with pointer input.
    // Each geometry is recorded before that sequence becomes publishable.
    frame_geometries: Mutex<[Option<PublishedFrameGeometry>; FRAME_GEOMETRY_HISTORY_COUNT]>,
    is_closed: AtomicBool,
    is_unlinked: AtomicBool,
}

// SAFETY: publication is serialized by ScreenCaptureKit's screen-output queue.
// The only concurrently read fields are immutable or atomic. The mapped atomic
// words are naturally aligned by the fixed protocol header.
unsafe impl Send for SharedFrameRing {}
unsafe impl Sync for SharedFrameRing {}

fn frame_publication_is_unavailable(word: u64) -> bool {
    word != FRAME_FAILURE_WORD && word & 0b11 == FRAME_UNAVAILABLE_SLOT
}

impl SharedFrameRing {
    fn create(width: usize, height: usize) -> anyhow::Result<Arc<Self>> {
        let layout = FrameLayout::new(width, height)?;
        let (name, descriptor_handle) = create_shared_memory()?;
        let result = Self::map(name, descriptor_handle, layout);
        unsafe {
            libc::close(descriptor_handle);
        }
        result.map(Arc::new)
    }

    fn map(name: CString, descriptor_handle: RawFd, layout: FrameLayout) -> anyhow::Result<Self> {
        Self::map_with(name, descriptor_handle, layout, || unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                layout.total_byte_count,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor_handle,
                0,
            )
        })
    }

    fn map_with<F>(
        name: CString,
        descriptor_handle: RawFd,
        layout: FrameLayout,
        map: F,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce() -> *mut c_void,
    {
        if unsafe { libc::ftruncate(descriptor_handle, layout.total_byte_count as libc::off_t) }
            != 0
        {
            unlink_shared_memory(&name);
            return Err(std::io::Error::last_os_error()).context("could not size frame ring");
        }
        let raw_mapping = map();
        let Some(mapping) = NonNull::new(raw_mapping.cast::<u8>())
            .filter(|pointer| pointer.as_ptr().cast::<c_void>() != libc::MAP_FAILED)
        else {
            let error = std::io::Error::last_os_error();
            unlink_shared_memory(&name);
            return Err(error).context("could not map frame ring");
        };

        let ring = Self {
            name,
            mapping,
            layout,
            next_sequence: AtomicU64::new(0),
            frame_geometries: Mutex::new([None; FRAME_GEOMETRY_HISTORY_COUNT]),
            is_closed: AtomicBool::new(false),
            is_unlinked: AtomicBool::new(false),
        };
        ring.initialize_header();
        Ok(ring)
    }

    fn descriptor(&self) -> FrameTransportDescriptor {
        FrameTransportDescriptor {
            shared_memory_name: self.name.to_string_lossy().into_owned(),
            width: self.layout.width,
            height: self.layout.height,
            bytes_per_row: self.layout.bytes_per_row,
            slot_count: FRAME_SLOT_COUNT,
            shared_memory_byte_count: self.layout.total_byte_count,
        }
    }

    fn initialize_header(&self) {
        unsafe {
            std::ptr::write_bytes(self.mapping.as_ptr(), 0, FRAME_HEADER_BYTE_COUNT);
            self.write_header_value(0, FRAME_MAGIC);
            self.write_header_value(4, FRAME_VERSION);
            self.write_header_value(8, self.layout.width as u32);
            self.write_header_value(12, self.layout.height as u32);
            self.write_header_value(16, self.layout.bytes_per_row as u32);
            self.write_header_value(20, FRAME_SLOT_COUNT as u32);
            self.write_header_value(24, self.layout.total_byte_count as u64);
            self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .store(0, Ordering::Release);
            for slot in 0..FRAME_SLOT_COUNT {
                self.atomic_word(FRAME_SLOT_VERSION_OFFSET + slot * FRAME_SLOT_VERSION_STRIDE)
                    .store(0, Ordering::Release);
            }
        }
    }

    unsafe fn write_header_value<T: Copy>(&self, offset: usize, value: T) {
        unsafe {
            self.mapping
                .as_ptr()
                .add(offset)
                .cast::<T>()
                .write_unaligned(value);
        }
    }

    unsafe fn atomic_word(&self, offset: usize) -> &AtomicU64 {
        unsafe { &*self.mapping.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    fn publish(
        &self,
        source: &[u8],
        source_bytes_per_row: usize,
        geometry: CapturedFrameGeometry,
    ) -> anyhow::Result<u64> {
        if self.is_closed.load(Ordering::Acquire) {
            bail!("frame ring is closed");
        }
        let publication = unsafe { self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET) };
        let previous_published_word = publication.load(Ordering::Acquire);
        if previous_published_word == FRAME_FAILURE_WORD {
            bail!("frame ring producer has failed");
        }
        if source_bytes_per_row < self.layout.bytes_per_row
            || source.len() < source_bytes_per_row.saturating_mul(self.layout.height)
        {
            bail!("captured frame has inconsistent row geometry");
        }
        let sequence = self
            .next_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(1)
                    .filter(|next| *next <= (i64::MAX as u64 >> 2))
            })
            .map_err(|_| anyhow!("frame sequence exceeded transport bounds"))?
            + 1;
        let slot = usize::try_from((sequence - 1) % FRAME_SLOT_COUNT as u64)
            .expect("triple-ring slot always fits usize");
        let completed_version = sequence
            .checked_mul(2)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| anyhow!("frame version exceeded transport bounds"))?;
        let published_word = (sequence << 2) | slot as u64;
        let slot_offset = self
            .layout
            .slot_offset(slot)
            .ok_or_else(|| anyhow!("invalid frame slot"))?;

        unsafe {
            self.atomic_word(FRAME_SLOT_VERSION_OFFSET + slot * FRAME_SLOT_VERSION_STRIDE)
                .swap(completed_version - 1, Ordering::AcqRel);
            let destination = self.mapping.as_ptr().add(slot_offset);
            if source_bytes_per_row == self.layout.bytes_per_row {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    destination,
                    self.layout.slot_byte_count,
                );
            } else {
                for row in 0..self.layout.height {
                    std::ptr::copy_nonoverlapping(
                        source.as_ptr().add(row * source_bytes_per_row),
                        destination.add(row * self.layout.bytes_per_row),
                        self.layout.bytes_per_row,
                    );
                }
            }
            self.atomic_word(FRAME_SLOT_VERSION_OFFSET + slot * FRAME_SLOT_VERSION_STRIDE)
                .store(completed_version, Ordering::Release);
        }
        let geometry_slot = usize::try_from((sequence - 1) % FRAME_GEOMETRY_HISTORY_COUNT as u64)
            .expect("bounded geometry history slot always fits usize");
        let mut frame_geometries = self
            .frame_geometries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        frame_geometries[geometry_slot] = Some(PublishedFrameGeometry { sequence, geometry });
        if publication
            .compare_exchange(
                previous_published_word,
                published_word,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_err()
        {
            frame_geometries[geometry_slot] = None;
            bail!("frame ring publication changed concurrently");
        }
        drop(frame_geometries);
        fence(Ordering::SeqCst);
        post_frame_notification(&self.name)?;
        Ok(sequence)
    }

    fn geometry_for_published_sequence(&self, sequence: u64) -> Option<CapturedFrameGeometry> {
        if sequence == 0 {
            return None;
        }
        let published_word = unsafe {
            self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .load(Ordering::Acquire)
        };
        if published_word == FRAME_FAILURE_WORD
            || frame_publication_is_unavailable(published_word)
            || (published_word >> 2) < sequence
        {
            return None;
        }
        let geometry_slot =
            usize::try_from((sequence - 1) % FRAME_GEOMETRY_HISTORY_COUNT as u64).ok()?;
        self.frame_geometries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[geometry_slot]
            .filter(|geometry| geometry.sequence == sequence)
            .map(|geometry| geometry.geometry)
    }

    fn is_available(&self) -> bool {
        let published_word = unsafe {
            self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .load(Ordering::Acquire)
        };
        published_word != FRAME_FAILURE_WORD
            && !frame_publication_is_unavailable(published_word)
            && published_word >> 2 > 0
    }

    fn mark_unavailable(&self) -> anyhow::Result<()> {
        let publication = unsafe { self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET) };
        let mut frame_geometries = self
            .frame_geometries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut current = publication.load(Ordering::Acquire);
        loop {
            if current == FRAME_FAILURE_WORD || frame_publication_is_unavailable(current) {
                return Ok(());
            }
            frame_geometries.fill(None);
            let unavailable_word = (current & !0b11) | FRAME_UNAVAILABLE_SLOT;
            match publication.compare_exchange(
                current,
                unavailable_word,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(updated) => current = updated,
            }
        }
        drop(frame_geometries);
        fence(Ordering::SeqCst);
        post_frame_notification(&self.name)
    }

    fn mark_failed(&self) -> anyhow::Result<()> {
        let publication = unsafe { self.atomic_word(FRAME_PUBLISHED_WORD_OFFSET) };
        if publication.swap(FRAME_FAILURE_WORD, Ordering::AcqRel) == FRAME_FAILURE_WORD {
            return Ok(());
        }
        fence(Ordering::SeqCst);
        post_frame_notification(&self.name)
    }

    fn acknowledge_attachment(&self) -> bool {
        if self.is_unlinked.swap(true, Ordering::AcqRel) {
            return true;
        }
        if unlink_shared_memory(&self.name) {
            true
        } else {
            self.is_unlinked.store(false, Ordering::Release);
            false
        }
    }
}

impl Drop for SharedFrameRing {
    fn drop(&mut self) {
        if self.is_closed.swap(true, Ordering::AcqRel) {
            return;
        }
        unsafe {
            libc::munmap(
                self.mapping.as_ptr().cast::<c_void>(),
                self.layout.total_byte_count,
            );
        }
        if !self.is_unlinked.swap(true, Ordering::AcqRel) {
            unlink_shared_memory(&self.name);
        }
    }
}

fn create_shared_memory() -> anyhow::Result<(CString, RawFd)> {
    for _ in 0..8 {
        let token = Uuid::new_v4().simple().to_string();
        let name = CString::new(format!("/cmux-sim-frame-{}", &token[..12]))
            .expect("generated shared-memory names contain no NUL");
        let handle = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            )
        };
        if handle >= 0 {
            if unsafe { libc::fcntl(handle, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                let error = std::io::Error::last_os_error();
                unsafe {
                    libc::close(handle);
                }
                unlink_shared_memory(&name);
                return Err(error).context("could not protect frame-ring descriptor");
            }
            return Ok((name, handle));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error).context("could not create frame ring");
        }
    }
    bail!("could not reserve a unique frame-ring name")
}

fn unlink_shared_memory(name: &CString) -> bool {
    let result = unsafe { libc::shm_unlink(name.as_ptr()) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
}

fn post_frame_notification(shared_memory_name: &CString) -> anyhow::Result<()> {
    let suffix = shared_memory_name
        .to_str()
        .context("frame-ring name is not UTF-8")?
        .strip_prefix('/')
        .ok_or_else(|| anyhow!("frame-ring name has no POSIX prefix"))?;
    let notification = CString::new(format!("com.cmux.simulator.frame.{suffix}"))
        .expect("generated notification names contain no NUL");
    let result = unsafe { notify_post(notification.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        bail!("could not signal frame publication")
    }
}

#[link(name = "System")]
extern "C" {
    fn notify_post(name: *const c_char) -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NormalizedContentRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy)]
struct PublishedFrameGeometry {
    sequence: u64,
    geometry: CapturedFrameGeometry,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CapturedFrameGeometry {
    content_rect: NormalizedContentRect,
    source_width: f64,
    source_height: f64,
}

impl Default for NormalizedContentRect {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }
    }
}

impl NormalizedContentRect {
    fn from_frame_info(info: &FrameInfo, frame_width: usize, frame_height: usize) -> Option<Self> {
        let rect = info.content_rect?;
        let scale_factor = info.scale_factor.unwrap_or(1.0);
        // Apple's SCStreamFrameInfoContentRect contract defines both the size
        // and location as points in the output surface. The separate
        // SCStreamFrameInfoScaleFactor is the pixel-to-point scale, so both the
        // origin and size must be converted to output pixels here.
        Self::from_frame_rect(
            rect.origin.x * scale_factor,
            rect.origin.y * scale_factor,
            rect.size.width * scale_factor,
            rect.size.height * scale_factor,
            frame_width,
            frame_height,
        )
    }

    fn from_frame_rect(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        frame_width: usize,
        frame_height: usize,
    ) -> Option<Self> {
        let frame_width = frame_width as f64;
        let frame_height = frame_height as f64;
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || frame_width <= 0.0
            || frame_height <= 0.0
        {
            return None;
        }
        let rect = Self {
            x: (x / frame_width).clamp(0.0, 1.0),
            y: (y / frame_height).clamp(0.0, 1.0),
            width: (width / frame_width).clamp(0.0, 1.0),
            height: (height / frame_height).clamp(0.0, 1.0),
        };
        (rect.width > 0.0 && rect.height > 0.0).then_some(rect)
    }

    fn source_point(self, x: f64, y: f64) -> Option<(f64, f64)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let source_x = (x - self.x) / self.width;
        let source_y = (y - self.y) / self.height;
        ((0.0..=1.0).contains(&source_x) && (0.0..=1.0).contains(&source_y))
            .then_some((source_x, source_y))
    }
}

impl CapturedFrameGeometry {
    fn from_frame_info(
        info: Option<&FrameInfo>,
        frame_width: usize,
        frame_height: usize,
        fallback_source_width: f64,
        fallback_source_height: f64,
    ) -> Self {
        let content_rect = info
            .and_then(|info| {
                NormalizedContentRect::from_frame_info(info, frame_width, frame_height)
            })
            .unwrap_or_default();
        let source_size = info
            .and_then(Self::source_size_from_frame_info)
            .unwrap_or((fallback_source_width, fallback_source_height));
        Self {
            content_rect,
            source_width: source_size.0,
            source_height: source_size.1,
        }
    }

    fn source_size_from_frame_info(info: &FrameInfo) -> Option<(f64, f64)> {
        let rect = info.content_rect?;
        let content_scale = info.content_scale?;
        if !content_scale.is_finite() || content_scale <= 0.0 {
            return None;
        }
        // SCStreamFrameInfoContentScale maps original content points into the
        // content rectangle's surface points. Inverting it recovers the source
        // window size that produced this exact frame.
        let width = rect.size.width / content_scale;
        let height = rect.size.height / content_scale;
        (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
            .then_some((width, height))
    }

    fn content_rect_for_target(
        self,
        target_width: f64,
        target_height: f64,
    ) -> Option<NormalizedContentRect> {
        if !target_width.is_finite() || !target_height.is_finite() {
            return None;
        }
        let width_matches =
            (target_width - self.source_width).abs() <= SOURCE_SIZE_MATCH_TOLERANCE_POINTS;
        let height_matches =
            (target_height - self.source_height).abs() <= SOURCE_SIZE_MATCH_TOLERANCE_POINTS;
        (width_matches && height_matches).then_some(self.content_rect)
    }
}

struct CaptureFrameState {
    ring: Arc<SharedFrameRing>,
    failed: AtomicBool,
    input_state: Arc<ApplicationSurfaceInputState>,
    target_window_id: u32,
    target_process_id: i32,
    fallback_source_width: f64,
    fallback_source_height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFrameDisposition {
    Publish,
    Preserve,
    Invalidate,
}

fn capture_frame_disposition(status: Option<SCFrameStatus>) -> CaptureFrameDisposition {
    match status {
        None | Some(SCFrameStatus::Complete | SCFrameStatus::Started) => {
            CaptureFrameDisposition::Publish
        }
        Some(SCFrameStatus::Idle) => CaptureFrameDisposition::Preserve,
        Some(SCFrameStatus::Blank | SCFrameStatus::Suspended | SCFrameStatus::Stopped) => {
            CaptureFrameDisposition::Invalidate
        }
    }
}

pub(crate) fn capture_frame_status_is_publishable(status: Option<SCFrameStatus>) -> bool {
    capture_frame_disposition(status) == CaptureFrameDisposition::Publish
}

impl CaptureFrameState {
    fn mark_failed(&self) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.ring.mark_failed();
    }

    fn mark_unavailable(&self) {
        let _dispatch = self.input_state.lock_dispatch();
        if self.ring.mark_unavailable().is_err() {
            drop(_dispatch);
            self.mark_failed();
            return;
        }
        self.input_state
            .release_pressed_locked(self.target_process_id, self.target_window_id);
    }

    fn receive(&self, sample: screencapturekit::cm::CMSampleBuffer) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        match capture_frame_disposition(sample.frame_status()) {
            CaptureFrameDisposition::Publish => {}
            CaptureFrameDisposition::Preserve => return,
            CaptureFrameDisposition::Invalidate => {
                self.mark_unavailable();
                return;
            }
        }
        let Some(pixel_buffer) = sample.image_buffer() else {
            return;
        };
        if pixel_buffer.pixel_format() != BGRA_PIXEL_FORMAT
            || pixel_buffer.width() != self.ring.layout.width
            || pixel_buffer.height() != self.ring.layout.height
        {
            self.mark_failed();
            return;
        }
        let frame_info = sample.frame_info();
        let geometry = CapturedFrameGeometry::from_frame_info(
            frame_info.as_ref(),
            pixel_buffer.width(),
            pixel_buffer.height(),
            self.fallback_source_width,
            self.fallback_source_height,
        );
        let Ok(guard) = pixel_buffer.lock_read_only() else {
            self.mark_failed();
            return;
        };
        if self
            .ring
            .publish(guard.as_slice(), guard.bytes_per_row(), geometry)
            .is_err()
        {
            self.mark_failed();
        }
    }
}

struct ApplicationSurfaceSession {
    target_window_id: u32,
    target_process_id: i32,
    target_uses_chromium_background_preparation: bool,
    stream: SCStream,
    frame_state: Arc<CaptureFrameState>,
    input_state: Arc<ApplicationSurfaceInputState>,
}

impl Drop for ApplicationSurfaceSession {
    fn drop(&mut self) {
        self.input_state
            .deactivate(self.target_process_id, self.target_window_id);
        if let Err(error) = self.stream.stop_capture() {
            tracing::warn!(%error, "application surface capture did not stop cleanly");
        }
    }
}

#[derive(Default)]
struct ApplicationSurfaceManager {
    sessions: HashMap<String, ApplicationSurfaceSession>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ApplicationSurfacePointerDelivery {
    screen_x: f64,
    screen_y: f64,
    local_x: f64,
    local_y: f64,
    modifiers: u64,
    click_count: i64,
    group_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ApplicationSurfacePointerRelease {
    kind: &'static str,
    delivery: ApplicationSurfacePointerDelivery,
}

#[derive(Default)]
struct ApplicationSurfaceKeyboardState {
    pressed_key_codes: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApplicationSurfacePointerTransition {
    group_id: i64,
    should_prepare_background: bool,
}

#[derive(Default)]
struct ApplicationSurfacePointerButtonState {
    group_id: Option<i64>,
    last_completed_click_count: i64,
    pressed_delivery: Option<ApplicationSurfacePointerDelivery>,
}

#[derive(Default)]
struct ApplicationSurfacePointerState {
    left: ApplicationSurfacePointerButtonState,
    right: ApplicationSurfacePointerButtonState,
}

#[derive(Default)]
struct ApplicationSurfaceScrollState {
    remainder_x: f64,
    remainder_y: f64,
}

impl ApplicationSurfaceScrollState {
    fn consume(&mut self, delta_x: f64, delta_y: f64) -> Option<(i32, i32)> {
        if !delta_x.is_finite() || !delta_y.is_finite() {
            return None;
        }
        let (wheel_x, remainder_x) = Self::consume_axis(self.remainder_x + delta_x);
        let (wheel_y, remainder_y) = Self::consume_axis(self.remainder_y + delta_y);
        self.remainder_x = remainder_x;
        self.remainder_y = remainder_y;
        Some((wheel_x, wheel_y))
    }

    fn consume_axis(value: f64) -> (i32, f64) {
        let bounded = value.clamp(i32::MIN as f64, i32::MAX as f64);
        let whole = bounded.trunc() as i32;
        let remainder = if bounded == value {
            value - f64::from(whole)
        } else {
            0.0
        };
        (whole, remainder)
    }
}

#[derive(Default)]
struct ApplicationSurfaceInputState {
    dispatch: Mutex<()>,
    pointer: Mutex<ApplicationSurfacePointerState>,
    scroll: Mutex<ApplicationSurfaceScrollState>,
    keyboard: Mutex<ApplicationSurfaceKeyboardState>,
    active: AtomicBool,
}

impl ApplicationSurfaceInputState {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn lock_dispatch(&self) -> std::sync::MutexGuard<'_, ()> {
        self.dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn deactivate(&self, process_id: i32, window_id: u32) {
        let _dispatch = self.lock_dispatch();
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.release_pressed_locked(process_id, window_id);
        self.active.store(false, Ordering::Release);
    }

    fn release_pressed_locked(&self, process_id: i32, window_id: u32) {
        self.release_pressed_locked_with(
            |release| {
                if let Err(error) = post_mouse_delivery(
                    process_id,
                    window_id,
                    release.kind,
                    release.delivery,
                    false,
                ) {
                    tracing::warn!(
                        %error,
                        kind = release.kind,
                        "application surface pointer release did not post cleanly"
                    );
                }
            },
            |key_code| {
                let Some(target) = ApplicationSurfaceKeyboardTarget::new(window_id, process_id)
                else {
                    return;
                };
                if let Err(error) = post_key(target, key_code, false, 0) {
                    tracing::warn!(
                        %error,
                        key_code,
                        "application surface key release did not post cleanly"
                    );
                }
            },
        );
    }

    #[cfg(test)]
    fn release_pressed_with(
        &self,
        release_pointer: impl FnMut(ApplicationSurfacePointerRelease),
        release_key: impl FnMut(u16),
    ) {
        let _dispatch = self.lock_dispatch();
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.release_pressed_locked_with(release_pointer, release_key);
    }

    #[cfg(test)]
    fn deactivate_with(
        &self,
        release_pointer: impl FnMut(ApplicationSurfacePointerRelease),
        release_key: impl FnMut(u16),
    ) {
        let _dispatch = self.lock_dispatch();
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.release_pressed_locked_with(release_pointer, release_key);
        self.active.store(false, Ordering::Release);
    }

    fn release_pressed_locked_with(
        &self,
        mut release_pointer: impl FnMut(ApplicationSurfacePointerRelease),
        mut release_key: impl FnMut(u16),
    ) {
        let releases = self
            .pointer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_pressed_releases();
        for pending in releases {
            release_pointer(pending);
        }
        let key_releases = self
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_pressed_releases();
        for key_code in key_releases {
            release_key(key_code);
        }
    }
}

impl ApplicationSurfaceKeyboardState {
    fn record_delivery(&mut self, key_code: u16, key_down: bool) {
        if key_down {
            if !self.pressed_key_codes.contains(&key_code) {
                self.pressed_key_codes.push(key_code);
            }
        } else {
            self.pressed_key_codes
                .retain(|pressed| *pressed != key_code);
        }
    }

    fn take_pressed_releases(&mut self) -> Vec<u16> {
        let mut releases = std::mem::take(&mut self.pressed_key_codes);
        releases.reverse();
        releases
    }
}

impl ApplicationSurfacePointerState {
    fn transition_for(
        &mut self,
        kind: &str,
        click_count: i64,
    ) -> ApplicationSurfacePointerTransition {
        let click_count = click_count.clamp(1, 3);
        let button = match kind {
            "left_mouse_down" | "left_mouse_dragged" | "left_mouse_up" => {
                Some(&mut self.left)
            }
            "right_mouse_down" | "right_mouse_dragged" | "right_mouse_up" => {
                Some(&mut self.right)
            }
            _ => None,
        };
        let Some(button) = button else {
            let group_id = self
                .left
                .pressed_delivery
                .or(self.right.pressed_delivery)
                .map(|delivery| delivery.group_id)
                .unwrap_or_else(next_click_group_id);
            return ApplicationSurfacePointerTransition {
                group_id,
                should_prepare_background: false,
            };
        };

        let is_down = matches!(kind, "left_mouse_down" | "right_mouse_down");
        let continues_click_series = is_down
            && button.group_id.is_some()
            && button.pressed_delivery.is_none()
            && click_count > 1
            && click_count == button.last_completed_click_count + 1;
        if is_down && !continues_click_series {
            button.group_id = Some(next_click_group_id());
            button.last_completed_click_count = 0;
        }
        let group_id = *button.group_id.get_or_insert_with(next_click_group_id);
        if matches!(kind, "left_mouse_up" | "right_mouse_up") {
            button.last_completed_click_count = click_count;
        }
        ApplicationSurfacePointerTransition {
            group_id,
            should_prepare_background: is_down && !continues_click_series,
        }
    }

    fn record_delivery(
        &mut self,
        kind: &str,
        delivery: ApplicationSurfacePointerDelivery,
    ) {
        match kind {
            "left_mouse_down" | "left_mouse_dragged" => {
                self.left.pressed_delivery = Some(delivery);
            }
            "left_mouse_up" => {
                self.left.pressed_delivery = None;
            }
            "right_mouse_down" | "right_mouse_dragged" => {
                self.right.pressed_delivery = Some(delivery);
            }
            "right_mouse_up" => {
                self.right.pressed_delivery = None;
            }
            _ => {}
        }
    }

    fn continued_delivery(
        &mut self,
        kind: &str,
        modifiers: u64,
        click_count: i64,
    ) -> Option<ApplicationSurfacePointerDelivery> {
        let previous = match kind {
            "left_mouse_dragged" | "left_mouse_up" => self.left.pressed_delivery,
            "right_mouse_dragged" | "right_mouse_up" => self.right.pressed_delivery,
            _ => None,
        }?;
        let transition = self.transition_for(kind, click_count);
        Some(ApplicationSurfacePointerDelivery {
            modifiers,
            click_count: click_count.clamp(1, 3),
            group_id: transition.group_id,
            ..previous
        })
    }

    fn take_pressed_releases(&mut self) -> Vec<ApplicationSurfacePointerRelease> {
        let mut releases = Vec::with_capacity(2);
        if let Some(delivery) = self.left.pressed_delivery.take() {
            releases.push(ApplicationSurfacePointerRelease {
                kind: "left_mouse_up",
                delivery,
            });
        }
        if let Some(delivery) = self.right.pressed_delivery.take() {
            releases.push(ApplicationSurfacePointerRelease {
                kind: "right_mouse_up",
                delivery,
            });
        }
        self.left.group_id = None;
        self.left.last_completed_click_count = 0;
        self.right.group_id = None;
        self.right.last_completed_click_count = 0;
        releases
    }
}

fn next_click_group_id() -> i64 {
    static NEXT_CLICK_GROUP_ID: AtomicU64 = AtomicU64::new(1);
    (NEXT_CLICK_GROUP_ID.fetch_add(1, Ordering::Relaxed) & i64::MAX as u64) as i64
}

fn manager() -> &'static Mutex<ApplicationSurfaceManager> {
    static MANAGER: OnceLock<Mutex<ApplicationSurfaceManager>> = OnceLock::new();
    MANAGER.get_or_init(|| Mutex::new(ApplicationSurfaceManager::default()))
}

pub fn list_windows() -> anyhow::Result<Vec<ApplicationWindow>> {
    require_application_surface_permissions(ApplicationSurfacePermissionUse::WindowListing)?;
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(false)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|error| anyhow!("could not enumerate application windows: {error}"))?;
    let helper_pid = std::process::id() as i32;
    let mut windows = content
        .windows()
        .into_iter()
        .filter_map(|window| {
            let owner = window.owning_application()?;
            let frame = window.frame();
            if window.window_id() == 0
                || owner.process_id() == helper_pid
                || window.window_layer() != 0
                || frame.size.width < 64.0
                || frame.size.height < 64.0
            {
                return None;
            }
            let owner_name = owner.application_name();
            let title = window
                .title()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| owner_name.clone());
            Some(ApplicationWindow {
                window_id: window.window_id(),
                process_id: owner.process_id(),
                owner: owner_name,
                title,
                width: frame.size.width,
                height: frame.size.height,
            })
        })
        .collect::<Vec<_>>();
    windows.sort_by(|left, right| {
        left.owner
            .to_lowercase()
            .cmp(&right.owner.to_lowercase())
            .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
            .then_with(|| left.window_id.cmp(&right.window_id))
    });
    Ok(windows)
}

#[derive(Debug)]
enum BoundedApplicationSurfaceStartError<StartError> {
    Start(StartError),
    TimedOut,
    TaskFailed(tokio::task::JoinError),
    OwnerStopped,
}

fn application_surface_start_gate() -> Arc<tokio::sync::Mutex<()>> {
    static GATE: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

async fn bounded_application_surface_start_with<
    Value,
    StartError,
    SpawnStart,
    Cleanup,
    CleanupFuture,
>(
    timeout: std::time::Duration,
    gate: Arc<tokio::sync::Mutex<()>>,
    spawn_start: SpawnStart,
    cleanup: Cleanup,
) -> Result<Value, BoundedApplicationSurfaceStartError<StartError>>
where
    Value: Send + 'static,
    StartError: Send + 'static,
    SpawnStart: FnOnce() -> tokio::task::JoinHandle<Result<Value, StartError>> + Send + 'static,
    Cleanup: FnOnce(Value) -> CleanupFuture + Send + 'static,
    CleanupFuture: std::future::Future<Output = ()> + Send + 'static,
{
    let (response_sender, response_receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        let Ok(lifecycle_guard) = tokio::time::timeout_at(deadline, gate.lock_owned()).await else {
            let _ = response_sender.send(Err(BoundedApplicationSurfaceStartError::TimedOut));
            return;
        };
        let mut start_task = spawn_start();
        let outcome = match tokio::time::timeout_at(deadline, &mut start_task).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(BoundedApplicationSurfaceStartError::Start(error)),
            Ok(Err(error)) => Err(BoundedApplicationSurfaceStartError::TaskFailed(error)),
            Err(_) => {
                let _ = response_sender.send(Err(BoundedApplicationSurfaceStartError::TimedOut));
                if let Ok(Ok(value)) = start_task.await {
                    cleanup(value).await;
                }
                drop(lifecycle_guard);
                return;
            }
        };
        match response_sender.send(outcome) {
            Ok(()) => {}
            Err(Ok(value)) => cleanup(value).await,
            Err(Err(_)) => {}
        }
        drop(lifecycle_guard);
    });
    response_receiver
        .await
        .unwrap_or(Err(BoundedApplicationSurfaceStartError::OwnerStopped))
}

pub async fn start(
    request: ApplicationSurfaceStartRequest,
) -> anyhow::Result<ApplicationSurfaceStartResult> {
    match bounded_application_surface_start_with(
        APPLICATION_SURFACE_START_TIMEOUT,
        application_surface_start_gate(),
        move || tokio::task::spawn_blocking(move || start_blocking(request)),
        |result: ApplicationSurfaceStartResult| async move {
            let session_id = result.session_id;
            let _ = tokio::task::spawn_blocking(move || stop(&session_id)).await;
        },
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(BoundedApplicationSurfaceStartError::Start(error)) => Err(error),
        Err(BoundedApplicationSurfaceStartError::TimedOut) => {
            Err(ApplicationSurfaceError::CaptureFailed.into())
        }
        Err(BoundedApplicationSurfaceStartError::TaskFailed(error)) => {
            Err(anyhow!("application-capture task failed: {error}"))
        }
        Err(BoundedApplicationSurfaceStartError::OwnerStopped) => {
            Err(anyhow!("application-capture lifecycle owner stopped"))
        }
    }
}

fn start_blocking(
    request: ApplicationSurfaceStartRequest,
) -> anyhow::Result<ApplicationSurfaceStartResult> {
    require_application_surface_permissions(ApplicationSurfacePermissionUse::CaptureAndInput)?;
    if !(1..=120).contains(&request.frame_rate) || request.window_id == 0 || request.process_id <= 0
    {
        bail!("invalid application-surface target or frame rate");
    }

    let content = SCShareableContent::create()
        .with_on_screen_windows_only(false)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|error| anyhow!("could not enumerate application windows: {error}"))?;
    let source_window = content
        .windows()
        .into_iter()
        .find(|window| {
            window.window_id() == request.window_id
                && window
                    .owning_application()
                    .is_some_and(|owner| owner.process_id() == request.process_id)
        })
        .ok_or(ApplicationSurfaceError::WindowUnavailable)?;
    let source_frame = source_window.frame();
    let (width, height) = capture_pixel_size(source_frame.size.width, source_frame.size.height)?;
    let ring = SharedFrameRing::create(width, height)?;
    let input_state = Arc::new(ApplicationSurfaceInputState::new());
    let frame_state = Arc::new(CaptureFrameState {
        ring: ring.clone(),
        failed: AtomicBool::new(false),
        input_state: Arc::clone(&input_state),
        target_window_id: request.window_id,
        target_process_id: request.process_id,
        fallback_source_width: source_frame.size.width,
        fallback_source_height: source_frame.size.height,
    });

    let filter = SCContentFilter::create()
        .with_window(&source_window)
        .build();
    let interval = screencapturekit::cm::CMTime::new(1, request.frame_rate as i32);
    let configuration = SCStreamConfiguration::new()
        .with_width(width as u32)
        .with_height(height as u32)
        .with_minimum_frame_interval(&interval)
        .with_queue_depth(3)
        // The host already presents the local pointer over the pane. Capturing
        // the target window's synthetic pointer would render a second cursor.
        .with_shows_cursor(false)
        .with_pixel_format(PixelFormat::BGRA)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true);
    let callback_state = frame_state.clone();
    let error_state = frame_state.clone();
    let delegate = StreamCallbacks::new().on_error(move |_| {
        error_state.mark_failed();
    });
    let mut stream = SCStream::new_with_delegate(&filter, &configuration, delegate);
    stream
        .add_output_handler(
            move |sample, output_type| {
                if output_type == SCStreamOutputType::Screen {
                    callback_state.receive(sample);
                }
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| anyhow!("could not attach application capture output"))?;
    stream
        .start_capture()
        .map_err(|error| anyhow!("could not start application capture: {error}"))?;

    let session_id = Uuid::new_v4().to_string();
    let result = ApplicationSurfaceStartResult {
        session_id: session_id.clone(),
        frame_transport: ring.descriptor(),
    };
    let mut manager = manager()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    manager.sessions.insert(
        session_id,
        ApplicationSurfaceSession {
            target_window_id: request.window_id,
            target_process_id: request.process_id,
            target_uses_chromium_background_preparation:
                application_surface_target_uses_chromium_background_preparation(request.process_id),
            stream,
            frame_state,
            input_state,
        },
    );
    Ok(result)
}

fn require_application_surface_permissions(
    permission_use: ApplicationSurfacePermissionUse,
) -> anyhow::Result<()> {
    let status = permissions::current_status();
    if let Some(error) = missing_application_surface_permission(
        status.accessibility,
        status.screen_recording,
        permission_use,
    ) {
        return Err(error.into());
    }
    Ok(())
}

fn missing_application_surface_permission(
    accessibility: bool,
    screen_recording: bool,
    permission_use: ApplicationSurfacePermissionUse,
) -> Option<ApplicationSurfaceError> {
    if permission_use == ApplicationSurfacePermissionUse::CaptureAndInput && !accessibility {
        return Some(ApplicationSurfaceError::AccessibilityPermissionRequired);
    }
    (!screen_recording).then_some(ApplicationSurfaceError::ScreenRecordingPermissionRequired)
}

fn capture_pixel_size(width: f64, height: f64) -> anyhow::Result<(usize, usize)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        bail!("application window has invalid dimensions");
    }
    let scale = 2.0_f64
        .min(4096.0 / width.max(1.0))
        .min(2304.0 / height.max(1.0));
    let pixel_width = (width * scale).round().max(1.0) as usize;
    let pixel_height = (height * scale).round().max(1.0) as usize;
    FrameLayout::new(pixel_width, pixel_height)?;
    Ok((pixel_width, pixel_height))
}

pub fn stop(session_id: &str) -> bool {
    let session = manager()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .sessions
        .remove(session_id);
    let stopped = session.is_some();
    drop(session);
    stopped
}

pub fn acknowledge_attachment(session_id: &str) -> bool {
    let ring = manager()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .sessions
        .get(session_id)
        .map(|session| session.frame_state.ring.clone());
    ring.is_some_and(|ring| ring.acknowledge_attachment())
}

pub fn stop_all() {
    let sessions = {
        let mut manager = manager()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut manager.sessions)
    };
    drop(sessions);
}

pub fn send_event(event: ApplicationSurfaceEvent) -> anyhow::Result<()> {
    send_events(vec![event])
}

pub fn send_events(events: Vec<ApplicationSurfaceEvent>) -> anyhow::Result<()> {
    let session_id = validate_event_batch(&events)?.to_owned();
    let (
        target_window_id,
        target_process_id,
        target_uses_chromium_background_preparation,
        frame_state,
        input_state,
    ) = {
        let manager = manager()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = manager
            .sessions
            .get(&session_id)
            .ok_or(ApplicationSurfaceError::SessionUnavailable)?;
        (
            session.target_window_id,
            session.target_process_id,
            session.target_uses_chromium_background_preparation,
            session.frame_state.clone(),
            session.input_state.clone(),
        )
    };
    let _dispatch = input_state.lock_dispatch();
    if !input_state.active.load(Ordering::Acquire) {
        return Err(ApplicationSurfaceError::SessionUnavailable.into());
    }
    if !permissions::current_status().accessibility {
        return Err(ApplicationSurfaceError::AccessibilityPermissionRequired.into());
    }
    if frame_state.failed.load(Ordering::Acquire) {
        return Err(ApplicationSurfaceError::CaptureFailed.into());
    }
    if !frame_state.ring.is_available() {
        return Err(ApplicationSurfaceError::CaptureUnavailable.into());
    }
    let target = live_target(target_window_id, target_process_id)
        .ok_or(ApplicationSurfaceError::WindowUnavailable)?;
    let resolved_content = validate_event_deliveries(
        &events,
        |sequence| {
            frame_state
                .ring
                .geometry_for_published_sequence(sequence)
                .and_then(|geometry| {
                    geometry.content_rect_for_target(target.bounds.width, target.bounds.height)
                })
        },
        &input_state,
    )?;
    for (event, content) in events.into_iter().zip(resolved_content) {
        dispatch_event(
            event,
            target_window_id,
            target_process_id,
            target_uses_chromium_background_preparation,
            &target,
            content,
            &input_state,
        )?;
    }
    Ok(())
}

fn validate_event_batch(events: &[ApplicationSurfaceEvent]) -> anyhow::Result<&str> {
    if events.is_empty() || events.len() > MAXIMUM_APPLICATION_SURFACE_EVENT_BATCH_COUNT {
        bail!("application-surface event batch is outside supported bounds");
    }
    let session = events[0].session.trim();
    if session.is_empty()
        || session.len() > 128
        || events.iter().any(|event| event.session != session)
    {
        bail!("application-surface event batch must target one valid session");
    }
    for event in events {
        validate_event_shape(event)?;
    }
    Ok(session)
}

fn validate_event_shape(event: &ApplicationSurfaceEvent) -> anyhow::Result<()> {
    match event.kind.as_str() {
        "mouse_moved"
        | "left_mouse_down"
        | "left_mouse_up"
        | "left_mouse_dragged"
        | "right_mouse_down"
        | "right_mouse_up"
        | "right_mouse_dragged" => {
            event.required_frame_sequence()?;
            event.pointer_coordinates()?;
        }
        "scroll" => {
            event.required_frame_sequence()?;
            event.scroll_values()?;
        }
        "key" => {
            event.key_values()?;
        }
        "health" => {}
        _ => bail!("unsupported application-surface event"),
    }
    Ok(())
}

fn validate_event_deliveries(
    events: &[ApplicationSurfaceEvent],
    content_for_sequence: impl Fn(u64) -> Option<NormalizedContentRect>,
    input_state: &ApplicationSurfaceInputState,
) -> anyhow::Result<Vec<Option<NormalizedContentRect>>> {
    let pointer = input_state
        .pointer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut left_pressed = pointer.left.pressed_delivery.is_some();
    let mut right_pressed = pointer.right.pressed_delivery.is_some();
    drop(pointer);
    let mut resolved_content = Vec::with_capacity(events.len());

    for event in events {
        let kind = event.kind.as_str();
        let content = event.frame_sequence.and_then(&content_for_sequence);
        match kind {
            "mouse_moved"
            | "left_mouse_down"
            | "left_mouse_up"
            | "left_mouse_dragged"
            | "right_mouse_down"
            | "right_mouse_up"
            | "right_mouse_dragged" => {
                let (x, y) = event.pointer_coordinates()?;
                let is_inside_content = content
                    .and_then(|content| content.source_point(x, y))
                    .is_some();
                let can_continue_outside = match kind {
                    "left_mouse_dragged" | "left_mouse_up" => left_pressed,
                    "right_mouse_dragged" | "right_mouse_up" => right_pressed,
                    _ => false,
                };
                if !is_inside_content && !can_continue_outside {
                    return Err(ApplicationSurfaceError::PointOutsideContent.into());
                }
                match kind {
                    "left_mouse_down" | "left_mouse_dragged" => left_pressed = true,
                    "left_mouse_up" => left_pressed = false,
                    "right_mouse_down" | "right_mouse_dragged" => right_pressed = true,
                    "right_mouse_up" => right_pressed = false,
                    _ => {}
                }
            }
            "scroll" => {
                let (x, y, _, _) = event.scroll_values()?;
                content
                    .and_then(|content| content.source_point(x, y))
                    .ok_or(ApplicationSurfaceError::PointOutsideContent)?;
            }
            "key" | "health" => {}
            _ => bail!("unsupported application-surface event"),
        }
        resolved_content.push(content);
    }
    Ok(resolved_content)
}

fn dispatch_event(
    event: ApplicationSurfaceEvent,
    target_window_id: u32,
    target_process_id: i32,
    target_uses_chromium_background_preparation: bool,
    target: &windows::WindowInfo,
    content: Option<NormalizedContentRect>,
    input_state: &ApplicationSurfaceInputState,
) -> anyhow::Result<()> {
    match event.kind.as_str() {
        "mouse_moved"
        | "left_mouse_down"
        | "left_mouse_up"
        | "left_mouse_dragged"
        | "right_mouse_down"
        | "right_mouse_up"
        | "right_mouse_dragged" => {
            let (x, y) = event.pointer_coordinates()?;
            let Some((source_x, source_y)) = content.and_then(|content| content.source_point(x, y))
            else {
                if matches!(
                    event.kind.as_str(),
                    "left_mouse_dragged"
                        | "left_mouse_up"
                        | "right_mouse_dragged"
                        | "right_mouse_up"
                ) {
                    return post_mouse_continuation(
                        target_process_id,
                        target_window_id,
                        event.kind.as_str(),
                        event.modifiers,
                        event.click_count,
                        &input_state.pointer,
                    );
                }
                return Err(ApplicationSurfaceError::PointOutsideContent.into());
            };
            let screen_x = target.bounds.x + source_x * target.bounds.width;
            let screen_y = target.bounds.y + source_y * target.bounds.height;
            post_mouse(
                target_process_id,
                target_window_id,
                event.kind.as_str(),
                screen_x,
                screen_y,
                source_x * target.bounds.width,
                source_y * target.bounds.height,
                event.modifiers,
                event.click_count,
                target_uses_chromium_background_preparation,
                &input_state.pointer,
            )
        }
        "scroll" => {
            let (x, y, delta_x, delta_y) = event.scroll_values()?;
            let (source_x, source_y) = content
                .and_then(|content| content.source_point(x, y))
                .ok_or(ApplicationSurfaceError::PointOutsideContent)?;
            post_scroll(
                target_process_id,
                target_window_id,
                target.bounds.x + source_x * target.bounds.width,
                target.bounds.y + source_y * target.bounds.height,
                source_x * target.bounds.width,
                source_y * target.bounds.height,
                delta_x,
                delta_y,
                event.modifiers,
                target_uses_chromium_background_preparation,
                &input_state.pointer,
                &input_state.scroll,
            )
        }
        "key" => {
            let (key_code, key_down) = event.key_values()?;
            let target = ApplicationSurfaceKeyboardTarget::new(target_window_id, target_process_id)
                .ok_or(ApplicationSurfaceError::WindowUnavailable)?;
            post_key(target, key_code, key_down, event.modifiers)?;
            input_state
                .keyboard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_delivery(key_code, key_down);
            Ok(())
        }
        "health" => Ok(()),
        _ => bail!("unsupported application-surface event"),
    }
}

fn live_target(window_id: u32, process_id: i32) -> Option<windows::WindowInfo> {
    windows::all_windows().into_iter().find(|window| {
        window.window_id == window_id
            && window.pid == process_id
            && window.bounds.width > 0.0
            && window.bounds.height > 0.0
    })
}

fn chromium_browser_bundle_id(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.google.Chrome" | "com.brave.Browser" | "com.microsoft.edgemac"
    ) || bundle_id.starts_with("com.google.Chrome.")
}

fn application_surface_target_uses_chromium_background_preparation(process_id: i32) -> bool {
    crate::apps::bundle_id_for_pid(process_id)
        .as_deref()
        .is_some_and(chromium_browser_bundle_id)
        || crate::browser::ElectronJs::is_electron(process_id)
}

fn mouse_event_requires_background_preparation(kind: &str, target_uses_chromium: bool) -> bool {
    target_uses_chromium && matches!(kind, "left_mouse_down" | "right_mouse_down")
}

#[allow(clippy::too_many_arguments)]
fn post_mouse(
    process_id: i32,
    window_id: u32,
    kind: &str,
    screen_x: f64,
    screen_y: f64,
    local_x: f64,
    local_y: f64,
    modifiers: u64,
    click_count: i64,
    target_uses_chromium_background_preparation: bool,
    pointer_state: &Mutex<ApplicationSurfacePointerState>,
) -> anyhow::Result<()> {
    let transition = pointer_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .transition_for(kind, click_count);
    let delivery = ApplicationSurfacePointerDelivery {
        screen_x,
        screen_y,
        local_x,
        local_y,
        modifiers,
        click_count,
        group_id: transition.group_id,
    };
    post_mouse_delivery(
        process_id,
        window_id,
        kind,
        delivery,
        transition.should_prepare_background
            && mouse_event_requires_background_preparation(
                kind,
                target_uses_chromium_background_preparation,
            ),
    )?;
    pointer_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record_delivery(kind, delivery);
    Ok(())
}

fn post_mouse_continuation(
    process_id: i32,
    window_id: u32,
    kind: &str,
    modifiers: u64,
    click_count: i64,
    pointer_state: &Mutex<ApplicationSurfacePointerState>,
) -> anyhow::Result<()> {
    let delivery = pointer_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .continued_delivery(kind, modifiers, click_count)
        .ok_or(ApplicationSurfaceError::PointOutsideContent)?;
    post_mouse_delivery(process_id, window_id, kind, delivery, false)?;
    pointer_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record_delivery(kind, delivery);
    Ok(())
}

fn post_mouse_delivery(
    process_id: i32,
    window_id: u32,
    kind: &str,
    delivery: ApplicationSurfacePointerDelivery,
    should_prepare_background: bool,
) -> anyhow::Result<()> {
    let (event_type, button, button_number) = match kind {
        "mouse_moved" => (CGEventType::MouseMoved, CGMouseButton::Left, 0),
        "left_mouse_down" => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
        "left_mouse_up" => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
        "left_mouse_dragged" => (CGEventType::LeftMouseDragged, CGMouseButton::Left, 0),
        "right_mouse_down" => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
        "right_mouse_up" => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
        "right_mouse_dragged" => (CGEventType::RightMouseDragged, CGMouseButton::Right, 1),
        _ => bail!("unsupported mouse event"),
    };
    let point = CGPoint::new(delivery.screen_x, delivery.screen_y);
    let flags = CGEventFlags::from_bits_truncate(delivery.modifiers);
    let source = if should_prepare_background {
        crate::input::mouse::prepare_chromium_background_gesture(
            process_id,
            delivery.screen_x,
            delivery.screen_y,
            delivery.local_x,
            delivery.local_y,
            window_id,
            delivery.group_id,
            flags,
        )?
    } else {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("could not create mouse event source"))?
    };
    let event = CGEvent::new_mouse_event(source, event_type, point, button)
        .map_err(|_| anyhow!("could not create mouse event"))?;
    event.set_flags(flags);
    crate::input::skylight::set_integer_field(
        event.as_ptr() as *mut c_void,
        0,
        application_surface_target_phase(kind),
    );
    let click_state = if kind == "mouse_moved" {
        0
    } else {
        delivery.click_count.clamp(1, 3)
    };
    let subtype = if matches!(kind, "left_mouse_dragged" | "right_mouse_dragged") {
        0
    } else {
        3
    };
    crate::input::mouse::post_mouse_event(
        process_id,
        &event,
        Some((delivery.local_x, delivery.local_y)),
        Some(window_id),
        Some(delivery.group_id),
        click_state,
        button_number,
        subtype,
    );
    Ok(())
}

fn application_surface_target_phase(kind: &str) -> i64 {
    if kind == "mouse_moved" {
        2
    } else {
        3
    }
}

#[allow(clippy::too_many_arguments)]
fn post_scroll(
    process_id: i32,
    window_id: u32,
    screen_x: f64,
    screen_y: f64,
    local_x: f64,
    local_y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: u64,
    target_uses_chromium_background_preparation: bool,
    pointer_state: &Mutex<ApplicationSurfacePointerState>,
    scroll_state: &Mutex<ApplicationSurfaceScrollState>,
) -> anyhow::Result<()> {
    let (wheel_x, wheel_y) = scroll_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .consume(delta_x, delta_y)
        .ok_or_else(|| anyhow!("application surface scroll delta is invalid"))?;
    if wheel_x == 0 && wheel_y == 0 {
        return Ok(());
    }

    post_mouse(
        process_id,
        window_id,
        "mouse_moved",
        screen_x,
        screen_y,
        local_x,
        local_y,
        modifiers,
        1,
        target_uses_chromium_background_preparation,
        pointer_state,
    )?;

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("could not create scroll event source"))?;
    let event = CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, wheel_y, wheel_x, 0)
        .map_err(|_| anyhow!("could not create scroll event"))?;
    event.set_flags(CGEventFlags::from_bits_truncate(modifiers));
    let pointer = event.as_ptr() as *mut c_void;
    unsafe { CGEventSetLocation(pointer, screen_x, screen_y) };
    crate::input::skylight::set_window_location(pointer, local_x, local_y);
    crate::input::skylight::set_integer_field(pointer, 40, process_id as i64);
    crate::input::skylight::set_integer_field(pointer, 51, window_id as i64);
    crate::input::skylight::set_integer_field(pointer, 91, window_id as i64);
    crate::input::skylight::set_integer_field(pointer, 92, window_id as i64);
    crate::input::skylight::post_to_pid(process_id as libc::pid_t, pointer, false);
    event.post_to_pid(process_id as libc::pid_t);
    Ok(())
}

fn post_key(
    target: ApplicationSurfaceKeyboardTarget,
    key_code: u16,
    key_down: bool,
    modifiers: u64,
) -> anyhow::Result<()> {
    if !crate::input::skylight::activate_without_raise(
        target.process_id as libc::pid_t,
        target.window_id,
    ) {
        return Err(ApplicationSurfaceError::WindowUnavailable.into());
    }
    std::thread::sleep(std::time::Duration::from_millis(8));
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("could not create keyboard event source"))?;
    let event = CGEvent::new_keyboard_event(source, key_code, key_down)
        .map_err(|_| anyhow!("could not create keyboard event"))?;
    event.set_flags(CGEventFlags::from_bits_truncate(modifiers));
    let pointer = event.as_ptr() as *mut c_void;
    for field in [51, 91, 92] {
        crate::input::skylight::set_integer_field(pointer, field, target.window_id as i64);
    }
    if !crate::input::skylight::post_to_pid(target.process_id as libc::pid_t, pointer, true) {
        return Err(ApplicationSurfaceError::WindowUnavailable.into());
    }
    Ok(())
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSetLocation(event: *mut c_void, x: f64, y: f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use screencapturekit::cm::{FrameInfo, SCFrameStatus};
    use std::sync::mpsc;
    use std::time::Duration;

    fn frame_geometry(content_rect: NormalizedContentRect) -> CapturedFrameGeometry {
        CapturedFrameGeometry {
            content_rect,
            source_width: 2.0,
            source_height: 2.0,
        }
    }

    #[test]
    fn window_listing_does_not_require_accessibility() {
        assert_eq!(
            missing_application_surface_permission(
                false,
                true,
                ApplicationSurfacePermissionUse::WindowListing,
            ),
            None
        );
        assert_eq!(
            missing_application_surface_permission(
                true,
                false,
                ApplicationSurfacePermissionUse::WindowListing,
            ),
            Some(ApplicationSurfaceError::ScreenRecordingPermissionRequired),
        );
        assert_eq!(
            missing_application_surface_permission(
                false,
                true,
                ApplicationSurfacePermissionUse::CaptureAndInput,
            ),
            Some(ApplicationSurfaceError::AccessibilityPermissionRequired),
        );
    }

    #[test]
    fn frame_layout_matches_cmux_triple_ring_protocol() {
        let layout = FrameLayout::new(100, 50).unwrap();
        assert_eq!(layout.bytes_per_row, 400);
        assert_eq!(layout.slot_byte_count, 20_000);
        assert_eq!(layout.total_byte_count % 4096, 0);
        assert_eq!(layout.slot_offset(0), Some(64));
        assert_eq!(layout.slot_offset(2), Some(40_064));
        assert_eq!(layout.slot_offset(3), None);
    }

    #[test]
    fn frame_ring_can_be_created_with_macos_shm_open() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let descriptor = ring.descriptor();
        assert_eq!(descriptor.width, 2);
        assert_eq!(descriptor.height, 2);
        assert_eq!(descriptor.slot_count, FRAME_SLOT_COUNT);

        let name = CString::new(descriptor.shared_memory_name).unwrap();
        let descriptor_handle = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        assert!(descriptor_handle >= 0);
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(
            unsafe { libc::fstat(descriptor_handle, metadata.as_mut_ptr()) },
            0
        );
        unsafe {
            libc::close(descriptor_handle);
        }
        let metadata = unsafe { metadata.assume_init() };
        assert_eq!(metadata.st_mode & 0o777, 0o600);
    }

    #[test]
    fn failed_frame_ring_notifies_reader_and_blocks_later_publication() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let frame = [0x5A; 16];

        ring.publish(&frame, 8, frame_geometry(NormalizedContentRect::default()))
            .unwrap();
        ring.mark_failed().unwrap();

        let published_word = unsafe {
            ring.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .load(Ordering::Acquire)
        };
        assert_eq!(published_word, FRAME_FAILURE_WORD);
        assert!(ring
            .publish(&frame, 8, frame_geometry(NormalizedContentRect::default()))
            .is_err());
        let published_word_after_rejected_frame = unsafe {
            ring.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .load(Ordering::Acquire)
        };
        assert_eq!(published_word_after_rejected_frame, FRAME_FAILURE_WORD);
    }

    #[test]
    fn unavailable_frame_ring_rejects_old_geometry_until_fresh_content() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let frame = [0x5A; 16];
        let first_sequence = ring
            .publish(&frame, 8, frame_geometry(NormalizedContentRect::default()))
            .unwrap();

        assert!(ring.is_available());
        assert!(ring
            .geometry_for_published_sequence(first_sequence)
            .is_some());
        ring.mark_unavailable().unwrap();
        assert!(!ring.is_available());
        assert!(ring
            .geometry_for_published_sequence(first_sequence)
            .is_none());

        let second_sequence = ring
            .publish(&frame, 8, frame_geometry(NormalizedContentRect::default()))
            .unwrap();
        assert!(ring.is_available());
        assert!(ring
            .geometry_for_published_sequence(first_sequence)
            .is_none());
        assert!(ring
            .geometry_for_published_sequence(second_sequence)
            .is_some());
    }

    #[test]
    fn attached_frame_ring_unlinks_its_persistent_name() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let name = ring.name.clone();
        let frame = [0x5A; 16];

        let handle_before_attach =
            unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        assert!(handle_before_attach >= 0);
        unsafe {
            libc::close(handle_before_attach);
        }

        assert!(ring.acknowledge_attachment());
        let handle_after_attach =
            unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        assert_eq!(handle_after_attach, -1);
        assert!(ring
            .publish(&frame, 8, frame_geometry(NormalizedContentRect::default()))
            .is_ok());
    }

    #[test]
    fn unknown_frame_ring_attachment_is_rejected() {
        assert!(!acknowledge_attachment("missing-session"));
    }

    #[test]
    fn mmap_failure_unlinks_shared_memory() {
        let layout = FrameLayout::new(2, 2).unwrap();
        let (name, descriptor_handle) = create_shared_memory().unwrap();
        let result = SharedFrameRing::map_with(
            name.clone(),
            descriptor_handle,
            layout,
            || libc::MAP_FAILED,
        );
        unsafe {
            libc::close(descriptor_handle);
        }

        assert!(result.is_err());
        let reopened = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        if reopened >= 0 {
            unsafe {
                libc::close(reopened);
            }
            unlink_shared_memory(&name);
        }
        assert_eq!(reopened, -1, "failed mmap must unlink the named object");
    }

    #[test]
    fn content_rect_rejects_letterbox_and_maps_live_content() {
        let rect =
            NormalizedContentRect::from_frame_rect(0.0, 250.0, 1000.0, 500.0, 1000, 1000).unwrap();
        assert_eq!(rect.source_point(0.5, 0.25), Some((0.5, 0.0)));
        assert_eq!(rect.source_point(0.5, 0.75), Some((0.5, 1.0)));
        assert_eq!(rect.source_point(0.5, 0.1), None);
    }

    #[test]
    fn fractional_scroll_deltas_accumulate_per_session() {
        let mut state = ApplicationSurfaceScrollState::default();

        assert_eq!(state.consume(0.25, -0.25), Some((0, 0)));
        assert_eq!(state.consume(0.25, -0.25), Some((0, 0)));
        assert_eq!(state.consume(0.25, -0.25), Some((0, 0)));
        assert_eq!(state.consume(0.25, -0.25), Some((1, -1)));
        assert_eq!(state.consume(f64::NAN, 0.0), None);
    }

    #[test]
    fn content_rect_scales_origin_and_size_without_applying_content_scale() {
        let info = FrameInfo {
            scale_factor: Some(2.0),
            content_scale: Some(0.5),
            content_rect: Some(screencapturekit::cg::CGRect::new(
                125.0, 125.0, 250.0, 250.0,
            )),
            ..FrameInfo::default()
        };

        let rect = NormalizedContentRect::from_frame_info(&info, 1000, 1000).unwrap();
        assert_eq!(rect.source_point(0.5, 0.5), Some((0.5, 0.5)));
        assert_eq!(rect.source_point(0.2, 0.5), None);
        assert_eq!(rect.source_point(0.5, 0.2), None);
    }

    #[test]
    fn captured_frame_geometry_recovers_source_size_from_content_scale() {
        let info = FrameInfo {
            scale_factor: Some(2.0),
            content_scale: Some(0.5),
            content_rect: Some(screencapturekit::cg::CGRect::new(
                125.0, 125.0, 250.0, 200.0,
            )),
            ..FrameInfo::default()
        };

        let geometry =
            CapturedFrameGeometry::from_frame_info(Some(&info), 1000, 1000, 800.0, 600.0);
        assert_eq!(geometry.source_width, 500.0);
        assert_eq!(geometry.source_height, 400.0);
    }

    #[test]
    fn captured_frame_geometry_rejects_input_after_source_resize() {
        let geometry = CapturedFrameGeometry {
            content_rect: NormalizedContentRect::default(),
            source_width: 800.0,
            source_height: 600.0,
        };

        assert_eq!(
            geometry.content_rect_for_target(800.0, 600.0),
            Some(NormalizedContentRect::default())
        );
        assert_eq!(geometry.content_rect_for_target(1200.0, 600.0), None);
    }

    #[test]
    fn published_frame_sequences_keep_their_own_input_geometry() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let frame = vec![0_u8; 16];
        let first = NormalizedContentRect::default();
        let second = NormalizedContentRect {
            x: 0.25,
            y: 0.25,
            width: 0.5,
            height: 0.5,
        };

        let first_geometry = frame_geometry(first);
        let second_geometry = frame_geometry(second);
        let first_sequence = ring.publish(&frame, 8, first_geometry).unwrap();
        let second_sequence = ring.publish(&frame, 8, second_geometry).unwrap();

        assert_eq!(
            ring.geometry_for_published_sequence(first_sequence),
            Some(first_geometry)
        );
        assert_eq!(
            ring.geometry_for_published_sequence(second_sequence),
            Some(second_geometry)
        );
        assert_eq!(
            ring.geometry_for_published_sequence(second_sequence + 1),
            None
        );
    }

    #[test]
    fn capture_size_is_bounded_without_changing_aspect_ratio() {
        assert_eq!(capture_pixel_size(800.0, 600.0).unwrap(), (1600, 1200));
        assert_eq!(capture_pixel_size(5000.0, 2500.0).unwrap(), (4096, 2048));
        assert!(capture_pixel_size(0.0, 600.0).is_err());
    }

    #[test]
    fn pointer_click_group_survives_multi_click_pairs() {
        let mut state = ApplicationSurfacePointerState::default();
        let first_down = state.transition_for("left_mouse_down", 1);
        assert!(first_down.should_prepare_background);
        assert_eq!(
            state.transition_for("left_mouse_up", 1).group_id,
            first_down.group_id
        );

        let second_down = state.transition_for("left_mouse_down", 2);
        assert_eq!(second_down.group_id, first_down.group_id);
        assert!(!second_down.should_prepare_background);
        assert_eq!(
            state.transition_for("left_mouse_up", 2).group_id,
            first_down.group_id
        );

        let next_single = state.transition_for("left_mouse_down", 1);
        assert_ne!(next_single.group_id, first_down.group_id);
        assert!(next_single.should_prepare_background);
    }

    #[test]
    fn out_of_content_release_reuses_the_last_delivered_pointer_position() {
        let mut state = ApplicationSurfacePointerState::default();
        let down = state.transition_for("left_mouse_down", 1);
        state.record_delivery(
            "left_mouse_down",
            ApplicationSurfacePointerDelivery {
                screen_x: 10.0,
                screen_y: 20.0,
                local_x: 3.0,
                local_y: 4.0,
                modifiers: 0,
                click_count: 1,
                group_id: down.group_id,
            },
        );

        let release = state
            .continued_delivery("left_mouse_up", 99, 2)
            .expect("a delivered press must have a fallback release");

        assert_eq!(release.screen_x, 10.0);
        assert_eq!(release.screen_y, 20.0);
        assert_eq!(release.local_x, 3.0);
        assert_eq!(release.local_y, 4.0);
        assert_eq!(release.modifiers, 99);
        assert_eq!(release.click_count, 2);
        assert_eq!(release.group_id, down.group_id);

        state.record_delivery("left_mouse_up", release);
        assert!(state.take_pressed_releases().is_empty());
    }

    #[test]
    fn deactivation_yields_a_release_for_each_pressed_button() {
        let input = ApplicationSurfaceInputState::new();
        let mut pointer = input
            .pointer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let left = pointer.transition_for("left_mouse_down", 1);
        pointer.record_delivery(
            "left_mouse_down",
            ApplicationSurfacePointerDelivery {
                screen_x: 10.0,
                screen_y: 20.0,
                local_x: 3.0,
                local_y: 4.0,
                modifiers: 0,
                click_count: 1,
                group_id: left.group_id,
            },
        );
        let right = pointer.transition_for("right_mouse_down", 1);
        pointer.record_delivery(
            "right_mouse_down",
            ApplicationSurfacePointerDelivery {
                screen_x: 30.0,
                screen_y: 40.0,
                local_x: 5.0,
                local_y: 6.0,
                modifiers: 0,
                click_count: 1,
                group_id: right.group_id,
            },
        );
        drop(pointer);

        let mut releases = Vec::new();
        input.deactivate_with(|release| releases.push(release), |_| {});

        assert!(!input.active.load(Ordering::Acquire));
        assert_eq!(releases.len(), 2);
        assert!(releases.iter().any(|release| {
            release.kind == "left_mouse_up"
                && release.delivery.screen_x == 10.0
                && release.delivery.group_id == left.group_id
        }));
        assert!(releases.iter().any(|release| {
            release.kind == "right_mouse_up"
                && release.delivery.screen_x == 30.0
                && release.delivery.group_id == right.group_id
        }));
    }

    #[test]
    fn deactivation_yields_key_releases_in_reverse_press_order() {
        let input = ApplicationSurfaceInputState::new();
        let mut keyboard = input
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keyboard.record_delivery(56, true);
        keyboard.record_delivery(0, true);
        keyboard.record_delivery(0, false);
        keyboard.record_delivery(1, true);
        drop(keyboard);

        let mut releases = Vec::new();
        input.deactivate_with(|_| {}, |key_code| releases.push(key_code));

        assert!(!input.active.load(Ordering::Acquire));
        assert_eq!(releases, vec![1, 56]);
    }

    #[test]
    fn temporary_capture_unavailability_releases_input_without_deactivation() {
        let input = ApplicationSurfaceInputState::new();
        let mut pointer = input
            .pointer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transition = pointer.transition_for("left_mouse_down", 1);
        pointer.record_delivery(
            "left_mouse_down",
            ApplicationSurfacePointerDelivery {
                screen_x: 10.0,
                screen_y: 20.0,
                local_x: 3.0,
                local_y: 4.0,
                modifiers: 0,
                click_count: 1,
                group_id: transition.group_id,
            },
        );
        drop(pointer);
        input
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_delivery(56, true);

        let mut pointer_releases = Vec::new();
        let mut key_releases = Vec::new();
        input.release_pressed_with(
            |release| pointer_releases.push(release),
            |key_code| key_releases.push(key_code),
        );

        assert!(input.active.load(Ordering::Acquire));
        assert_eq!(pointer_releases.len(), 1);
        assert_eq!(pointer_releases[0].kind, "left_mouse_up");
        assert_eq!(key_releases, vec![56]);
    }

    #[test]
    fn permanent_capture_failure_releases_delivered_input_once() {
        let ring = SharedFrameRing::create(2, 2).unwrap();
        let input = Arc::new(ApplicationSurfaceInputState::new());
        let mut pointer = input
            .pointer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transition = pointer.transition_for("left_mouse_down", 1);
        pointer.record_delivery(
            "left_mouse_down",
            ApplicationSurfacePointerDelivery {
                screen_x: 10.0,
                screen_y: 20.0,
                local_x: 3.0,
                local_y: 4.0,
                modifiers: 0,
                click_count: 1,
                group_id: transition.group_id,
            },
        );
        drop(pointer);
        input
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_delivery(56, true);
        let frame_state = CaptureFrameState {
            ring: Arc::clone(&ring),
            failed: AtomicBool::new(false),
            input_state: input,
            target_window_id: 42,
            target_process_id: 43,
            fallback_source_width: 2.0,
            fallback_source_height: 2.0,
        };
        let mut pointer_releases = Vec::new();
        let mut key_releases = Vec::new();

        frame_state.mark_failed_with(
            |release| pointer_releases.push(release),
            |key_code| key_releases.push(key_code),
        );
        frame_state.mark_failed_with(
            |release| pointer_releases.push(release),
            |key_code| key_releases.push(key_code),
        );

        assert_eq!(pointer_releases.len(), 1);
        assert_eq!(pointer_releases[0].kind, "left_mouse_up");
        assert_eq!(key_releases, vec![56]);
        let published_word = unsafe {
            ring.atomic_word(FRAME_PUBLISHED_WORD_OFFSET)
                .load(Ordering::Acquire)
        };
        assert_eq!(published_word, FRAME_FAILURE_WORD);
    }

    #[tokio::test]
    async fn timed_out_application_start_owns_cleanup_and_blocks_retry() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleaned_marker = Arc::clone(&cleaned);
        let (release_start_tx, release_start_rx) = tokio::sync::oneshot::channel();
        let first = bounded_application_surface_start_with(
            Duration::from_millis(10),
            Arc::clone(&gate),
            move || {
                tokio::spawn(async move {
                    release_start_rx.await.unwrap();
                    Ok::<String, ()>("late-session".to_owned())
                })
            },
            move |session| {
                let cleaned_marker = Arc::clone(&cleaned_marker);
                async move {
                    assert_eq!(session, "late-session");
                    cleaned_marker.store(true, Ordering::Release);
                }
            },
        )
        .await;
        assert!(matches!(
            first,
            Err(BoundedApplicationSurfaceStartError::TimedOut)
        ));

        let retry_starts = Arc::new(AtomicUsize::new(0));
        let retry_start_marker = Arc::clone(&retry_starts);
        let retry = bounded_application_surface_start_with(
            Duration::from_millis(10),
            Arc::clone(&gate),
            move || {
                retry_start_marker.fetch_add(1, Ordering::AcqRel);
                tokio::spawn(async { Ok::<String, ()>("retry".to_owned()) })
            },
            |_| async {},
        )
        .await;
        assert!(matches!(
            retry,
            Err(BoundedApplicationSurfaceStartError::TimedOut)
        ));
        assert_eq!(retry_starts.load(Ordering::Acquire), 0);

        release_start_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !cleaned.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let _released_guard = tokio::time::timeout(Duration::from_secs(1), gate.lock())
            .await
            .unwrap();
    }

    #[test]
    fn stopping_an_unknown_application_surface_reports_false() {
        assert!(!stop(&Uuid::new_v4().to_string()));
    }

    #[test]
    fn streamed_target_events_use_chromium_target_phase() {
        for kind in [
            "left_mouse_down",
            "left_mouse_up",
            "left_mouse_dragged",
            "right_mouse_down",
            "right_mouse_up",
            "right_mouse_dragged",
        ] {
            assert_eq!(application_surface_target_phase(kind), 3);
        }
        assert_eq!(application_surface_target_phase("mouse_moved"), 2);
    }

    #[test]
    fn application_surface_event_batches_are_bounded_and_session_scoped() {
        let event = |session: &str| ApplicationSurfaceEvent {
            session: session.to_owned(),
            kind: "health".to_owned(),
            frame_sequence: None,
            x: Some(0.0),
            y: Some(0.0),
            button: String::new(),
            key_code: Some(0),
            key_down: Some(false),
            modifiers: 0,
            click_count: 1,
            delta_x: Some(0.0),
            delta_y: Some(0.0),
        };

        assert!(validate_event_batch(&[event("one")]).is_ok());
        assert!(validate_event_batch(&[]).is_err());
        assert!(validate_event_batch(&[event("one"), event("two")]).is_err());
        assert!(
            validate_event_batch(
                &(0..=MAXIMUM_APPLICATION_SURFACE_EVENT_BATCH_COUNT)
                    .map(|_| event("one"))
                    .collect::<Vec<_>>()
            )
            .is_err()
        );

        let mut key_down = event("one");
        key_down.kind = "key".to_owned();
        key_down.key_down = Some(true);
        let mut unsupported = event("one");
        unsupported.kind = "unsupported".to_owned();
        assert!(validate_event_batch(&[key_down.clone(), unsupported]).is_err());

        let mut invalid_scroll = event("one");
        invalid_scroll.kind = "scroll".to_owned();
        invalid_scroll.delta_y = Some(f64::NAN);
        assert!(validate_event_batch(&[key_down, invalid_scroll]).is_err());
    }

    #[test]
    fn event_deserialization_rejects_omitted_kind_specific_fields() {
        for event in [
            serde_json::json!({
                "session": "one",
                "kind": "left_mouse_down",
            }),
            serde_json::json!({
                "session": "one",
                "kind": "mouse_moved",
                "x": 0.5,
            }),
            serde_json::json!({
                "session": "one",
                "kind": "mouse_moved",
                "x": 0.5,
                "y": 0.5,
            }),
            serde_json::json!({
                "session": "one",
                "kind": "scroll",
                "x": 0.5,
                "y": 0.5,
                "delta_x": 0.0,
            }),
            serde_json::json!({
                "session": "one",
                "kind": "key",
                "key_down": false,
            }),
            serde_json::json!({
                "session": "one",
                "kind": "key",
                "key_code": 0,
            }),
        ] {
            let request: ApplicationSurfaceEventBatchRequest =
                serde_json::from_value(serde_json::json!({
                    "events": [event],
                }))
                .unwrap();
            assert!(request.into_validated_events().is_err());
        }

        let request: ApplicationSurfaceEventBatchRequest =
            serde_json::from_value(serde_json::json!({
                "events": [{
                    "session": "one",
                    "kind": "key",
                    "key_code": 0,
                    "key_down": false,
                }],
            }))
            .unwrap();
        assert!(request.into_validated_events().is_ok());
    }

    #[test]
    fn event_batch_delivery_validation_rejects_an_invalid_suffix() {
        let event = |kind: &str, x: f64, y: f64| ApplicationSurfaceEvent {
            session: "one".to_owned(),
            kind: kind.to_owned(),
            frame_sequence: Some(1),
            x: Some(x),
            y: Some(y),
            button: String::new(),
            key_code: Some(0),
            key_down: Some(false),
            modifiers: 0,
            click_count: 1,
            delta_x: Some(0.0),
            delta_y: Some(0.0),
        };
        let content = NormalizedContentRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        let input = ApplicationSurfaceInputState::new();

        assert!(validate_event_deliveries(
            &[
                event("key", 0.0, 0.0),
                event("mouse_moved", 2.0, 0.5),
            ],
            |_| Some(content),
            &input,
        )
        .is_err());
        assert!(validate_event_deliveries(
            &[
                event("left_mouse_down", 0.5, 0.5),
                event("left_mouse_up", 2.0, 0.5),
            ],
            |_| Some(content),
            &input,
        )
        .is_ok());
        assert!(validate_event_deliveries(
            &[event("left_mouse_up", 2.0, 0.5)],
            |_| Some(content),
            &input,
        )
        .is_err());
    }

    #[test]
    fn event_delivery_resolves_geometry_from_each_displayed_frame_sequence() {
        let event = |frame_sequence: u64, x: f64, y: f64| ApplicationSurfaceEvent {
            session: "one".to_owned(),
            kind: "mouse_moved".to_owned(),
            frame_sequence: Some(frame_sequence),
            x: Some(x),
            y: Some(y),
            button: String::new(),
            key_code: Some(0),
            key_down: Some(false),
            modifiers: 0,
            click_count: 1,
            delta_x: Some(0.0),
            delta_y: Some(0.0),
        };
        let full_frame = NormalizedContentRect::default();
        let inset_frame = NormalizedContentRect {
            x: 0.25,
            y: 0.25,
            width: 0.5,
            height: 0.5,
        };
        let content_for_sequence = |sequence| match sequence {
            1 => Some(full_frame),
            2 => Some(inset_frame),
            _ => None,
        };
        let input = ApplicationSurfaceInputState::new();

        assert!(validate_event_deliveries(
            &[event(1, 0.1, 0.1), event(2, 0.1, 0.1)],
            content_for_sequence,
            &input,
        )
        .is_err());
        assert!(validate_event_deliveries(
            &[event(1, 0.1, 0.1), event(2, 0.5, 0.5)],
            content_for_sequence,
            &input,
        )
        .is_ok());
    }

    #[test]
    fn content_bearing_and_statusless_capture_frames_are_publishable() {
        // macOS 26 can omit SCFrameStatus while still supplying a valid pixel
        // buffer. `CaptureFrameState::receive` validates that buffer before
        // publishing, so a missing optional attachment must not discard it.
        assert!(capture_frame_status_is_publishable(None));
        assert!(capture_frame_status_is_publishable(Some(
            SCFrameStatus::Complete
        )));
        assert!(capture_frame_status_is_publishable(Some(
            SCFrameStatus::Started
        )));
        assert_eq!(
            capture_frame_disposition(Some(SCFrameStatus::Idle)),
            CaptureFrameDisposition::Preserve,
        );
        for status in [
            Some(SCFrameStatus::Blank),
            Some(SCFrameStatus::Suspended),
            Some(SCFrameStatus::Stopped),
        ] {
            assert!(!capture_frame_status_is_publishable(status));
            assert_eq!(
                capture_frame_disposition(status),
                CaptureFrameDisposition::Invalidate,
            );
        }
    }

    #[test]
    fn session_event_dispatch_is_serialized() {
        let input = Arc::new(ApplicationSurfaceInputState::default());
        let first = input.lock_dispatch();
        let second_input = input.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _second = second_input.lock_dispatch();
            entered_tx.send(()).unwrap();
        });

        assert!(entered_rx.recv_timeout(Duration::from_millis(30)).is_err());
        drop(first);
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn mouse_down_events_prepare_only_chromium_backgrounds() {
        assert!(mouse_event_requires_background_preparation(
            "left_mouse_down",
            true,
        ));
        assert!(mouse_event_requires_background_preparation(
            "right_mouse_down",
            true,
        ));
        assert!(!mouse_event_requires_background_preparation(
            "left_mouse_down",
            false,
        ));
        assert!(!mouse_event_requires_background_preparation(
            "right_mouse_down",
            false,
        ));
        for kind in [
            "mouse_moved",
            "left_mouse_dragged",
            "left_mouse_up",
            "right_mouse_dragged",
            "right_mouse_up",
        ] {
            assert!(!mouse_event_requires_background_preparation(kind, true));
        }
    }

    #[test]
    fn chromium_background_preparation_uses_explicit_bundle_identity() {
        for bundle_id in [
            "com.google.Chrome",
            "com.google.Chrome.app.example",
            "com.brave.Browser",
            "com.microsoft.edgemac",
        ] {
            assert!(chromium_browser_bundle_id(bundle_id), "{bundle_id}");
        }
        for bundle_id in ["com.apple.Safari", "com.apple.calculator"] {
            assert!(!chromium_browser_bundle_id(bundle_id), "{bundle_id}");
        }
    }

    #[test]
    fn keyboard_targets_keep_same_process_windows_distinct() {
        let first = ApplicationSurfaceKeyboardTarget::new(101, 44).unwrap();
        let second = ApplicationSurfaceKeyboardTarget::new(202, 44).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.window_id, 101);
        assert_eq!(second.window_id, 202);
        assert_eq!(first.process_id, second.process_id);
    }
}

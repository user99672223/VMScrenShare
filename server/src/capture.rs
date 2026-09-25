//! Kernel-level framebuffer capture from the vkms DRM device.
//!
//! Flow per frame:
//! 1. Read the primary plane of the CRTC driving the `Virtual-1` connector to get the current
//!    framebuffer id (`DRM_IOCTL_MODE_GETPLANE`).
//! 2. If the framebuffer changed since the last frame, `GETFB2` it to obtain the GEM handle,
//!    pitch and pixel format, export the handle as a PRIME dma-buf fd and `mmap` it. Xorg with
//!    `AccelMethod none` renders into one long-lived dumb buffer, so this happens once.
//! 3. Return a view onto the mapping; the caller converts it (see [`crate::convert`]).
//!
//! `GETFB2` only returns buffer handles to the DRM master or to a process with `CAP_SYS_ADMIN`;
//! `setup` grants that capability to the binary. If the handles come back empty we say so.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use drm::buffer::DrmFourcc;
use drm::control::{connector, crtc, framebuffer, plane, Device as ControlDevice};
use drm::Device;

/// An opened DRM card node.
pub struct Card {
    file: File,
    path: PathBuf,
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

impl Card {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Kernel driver name, e.g. `"vkms"`.
    pub fn driver_name(&self) -> Result<String> {
        let driver = self
            .get_driver()
            .with_context(|| format!("DRM_IOCTL_VERSION on {}", self.path.display()))?;
        Ok(driver.name().to_string_lossy().into_owned())
    }

    /// Names of all connectors on this card (`"Virtual-1"`, `"HDMI-A-1"`, ...).
    pub fn connector_names(&self) -> Result<Vec<String>> {
        let res = self
            .resource_handles()
            .context("DRM_IOCTL_MODE_GETRESOURCES")?;
        let mut names = Vec::new();
        for &handle in res.connectors() {
            if let Ok(info) = self.get_connector(handle, false) {
                names.push(connector_name(&info));
            }
        }
        Ok(names)
    }
}

/// `"<interface>-<index>"`, matching the kernel/Xorg naming.
pub fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// All `/dev/dri/card*` nodes, sorted.
pub fn card_paths() -> Vec<PathBuf> {
    let mut cards: Vec<PathBuf> = match std::fs::read_dir("/dev/dri") {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("card"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    cards.sort();
    cards
}

/// Finds the vkms card: `configured` if set, else the card whose driver is `vkms`, else any card
/// exposing a connector called `connector`.
pub fn find_card(configured: &str, connector: &str) -> Result<Card> {
    if !configured.is_empty() {
        return Card::open(Path::new(configured));
    }
    let paths = card_paths();
    if paths.is_empty() {
        bail!("no /dev/dri/card* nodes found: is the vkms module loaded? (sudo modprobe vkms)");
    }
    let mut seen = Vec::new();
    let mut by_connector = None;
    for path in &paths {
        let card = match Card::open(path) {
            Ok(c) => c,
            Err(e) => {
                seen.push(format!("{}: {e:#}", path.display()));
                continue;
            }
        };
        let driver = card.driver_name().unwrap_or_else(|_| "?".into());
        let connectors = card.connector_names().unwrap_or_default();
        seen.push(format!(
            "{}: driver {driver}, connectors {connectors:?}",
            path.display()
        ));
        if driver == "vkms" {
            return Ok(card);
        }
        if by_connector.is_none() && connectors.iter().any(|c| c == connector) {
            by_connector = Some(card);
        }
    }
    if let Some(card) = by_connector {
        tracing::warn!(
            "no card with driver vkms; using {} because it has connector {connector}",
            card.path().display()
        );
        return Ok(card);
    }
    bail!(
        "no DRM card with driver vkms or connector {connector} found. Cards seen:\n  {}",
        seen.join("\n  ")
    )
}

/// The connector/CRTC/plane triple that is currently scanning out.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub connector: connector::Handle,
    #[allow(dead_code)]
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// Why the display cannot be captured right now.
#[derive(Debug, thiserror::Error)]
pub enum DisplayState {
    #[error("connector {0} not found on {1}; connectors present: {2:?}")]
    NoConnector(String, PathBuf, Vec<String>),
    #[error("connector {0} has no active CRTC (no X server / mode set yet)")]
    Inactive(String),
    #[error("CRTC of {0} has no framebuffer (nothing scanned out yet)")]
    NoFramebuffer(String),
    #[error("no primary plane attached to the CRTC of {0}")]
    NoPlane(String),
}

/// Locates the active display behind `connector_name`.
pub fn locate_display(card: &Card, connector_name_wanted: &str) -> Result<DisplayInfo> {
    card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
        .context("enabling universal planes")?;
    let res = card
        .resource_handles()
        .context("DRM_IOCTL_MODE_GETRESOURCES")?;
    let mut names = Vec::new();
    let mut found = None;
    for &handle in res.connectors() {
        let info = card
            .get_connector(handle, false)
            .with_context(|| format!("GETCONNECTOR {handle:?}"))?;
        let name = connector_name(&info);
        if name == connector_name_wanted {
            found = Some(info);
        }
        names.push(name);
    }
    let conn = found.ok_or_else(|| {
        DisplayState::NoConnector(
            connector_name_wanted.into(),
            card.path().to_path_buf(),
            names,
        )
    })?;
    let name = connector_name_wanted.to_string();
    let encoder = conn
        .current_encoder()
        .ok_or_else(|| DisplayState::Inactive(name.clone()))?;
    let enc = card
        .get_encoder(encoder)
        .with_context(|| format!("GETENCODER {encoder:?}"))?;
    let crtc = enc
        .crtc()
        .ok_or_else(|| DisplayState::Inactive(name.clone()))?;
    let crtc_info = card
        .get_crtc(crtc)
        .with_context(|| format!("GETCRTC {crtc:?}"))?;
    let mode = crtc_info
        .mode()
        .ok_or_else(|| DisplayState::Inactive(name.clone()))?;
    if crtc_info.framebuffer().is_none() {
        return Err(DisplayState::NoFramebuffer(name).into());
    }

    let mut primary = None;
    let mut fallback = None;
    for handle in card.plane_handles().context("GETPLANERESOURCES")? {
        let pinfo = match card.get_plane(handle) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pinfo.crtc() != Some(crtc) {
            continue;
        }
        match plane_type(card, handle) {
            Ok(Some(t)) if t == "Primary" => {
                primary = Some(handle);
                break;
            }
            _ => {
                if pinfo.framebuffer().is_some() && fallback.is_none() {
                    fallback = Some(handle);
                }
            }
        }
    }
    let plane = primary
        .or(fallback)
        .ok_or_else(|| DisplayState::NoPlane(name.clone()))?;
    let (width, height) = mode.size();
    Ok(DisplayInfo {
        connector: conn.handle(),
        crtc,
        plane,
        width: u32::from(width),
        height: u32::from(height),
        refresh: mode.vrefresh(),
    })
}

/// Value of the plane's `type` enum property (`"Primary"`, `"Overlay"`, `"Cursor"`).
fn plane_type(card: &Card, plane: plane::Handle) -> Result<Option<String>> {
    let props = card.get_properties(plane)?;
    for (&prop, &raw) in props.iter() {
        let info = card.get_property(prop)?;
        if info.name().to_bytes() != b"type" {
            continue;
        }
        if let drm::control::property::ValueType::Enum(values) = info.value_type() {
            return Ok(values
                .get_value_from_raw_value(raw)
                .map(|e| e.name().to_string_lossy().into_owned()));
        }
        return Ok(None);
    }
    Ok(None)
}

/// A captured frame: a read-only view of the scanout buffer.
pub struct CapturedFrame<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    /// Row pitch in bytes.
    pub pitch: usize,
    pub format: DrmFourcc,
    /// True if the framebuffer object changed since the previous frame (new mapping).
    #[allow(dead_code)]
    pub remapped: bool,
}

struct Mapping {
    fb: framebuffer::Handle,
    width: u32,
    height: u32,
    pitch: u32,
    offset: usize,
    format: DrmFourcc,
    mmap: memmap2::Mmap,
    via: &'static str,
}

/// Reads frames from the framebuffer of one display.
pub struct FrameSource {
    card: Card,
    display: DisplayInfo,
    mapping: Option<Mapping>,
}

impl FrameSource {
    pub fn new(card: Card, display: DisplayInfo) -> Self {
        Self {
            card,
            display,
            mapping: None,
        }
    }

    #[allow(dead_code)]
    pub fn display(&self) -> &DisplayInfo {
        &self.display
    }

    #[allow(dead_code)]
    pub fn card(&self) -> &Card {
        &self.card
    }

    /// Returns the current scanout buffer contents.
    pub fn capture(&mut self) -> Result<CapturedFrame<'_>> {
        let pinfo = self
            .card
            .get_plane(self.display.plane)
            .context("GETPLANE (display went away?)")?;
        let fb = match pinfo.framebuffer() {
            Some(fb) => fb,
            None => {
                self.mapping = None;
                bail!(DisplayState::NoFramebuffer(format!(
                    "{:?}",
                    self.display.connector
                )));
            }
        };
        let mut remapped = false;
        if self.mapping.as_ref().map(|m| m.fb) != Some(fb) {
            self.mapping = Some(map_framebuffer(&self.card, fb)?);
            remapped = true;
            let m = self.mapping.as_ref().unwrap();
            tracing::info!(
                "mapped framebuffer {:?}: {}x{} {:?} pitch {} via {}",
                fb,
                m.width,
                m.height,
                m.format,
                m.pitch,
                m.via
            );
        }
        let m = self.mapping.as_ref().unwrap();
        let len = m.pitch as usize * m.height as usize;
        Ok(CapturedFrame {
            data: &m.mmap[m.offset..m.offset + len],
            width: m.width as usize,
            height: m.height as usize,
            pitch: m.pitch as usize,
            format: m.format,
            remapped,
        })
    }
}

/// Maps a framebuffer: GETFB2 → PRIME export → mmap, with a `MAP_DUMB` fallback.
fn map_framebuffer(card: &Card, fb: framebuffer::Handle) -> Result<Mapping> {
    let (buffer, width, height, pitch, offset, format) = match card.get_planar_framebuffer(fb) {
        Ok(info) => {
            let format = info.pixel_format();
            let buffer = info.buffers()[0];
            let (w, h) = info.size();
            (
                buffer,
                w,
                h,
                info.pitches()[0],
                info.offsets()[0] as usize,
                format,
            )
        }
        Err(e) => {
            tracing::debug!("GETFB2 failed ({e}), falling back to GETFB");
            let info = card
                .get_framebuffer(fb)
                .with_context(|| format!("GETFB {fb:?}"))?;
            let format = match (info.bpp(), info.depth()) {
                (32, 24) => DrmFourcc::Xrgb8888,
                (32, 32) => DrmFourcc::Argb8888,
                (bpp, depth) => {
                    bail!("unsupported legacy framebuffer format bpp {bpp} depth {depth}")
                }
            };
            let (w, h) = info.size();
            (info.buffer(), w, h, info.pitch(), 0, format)
        }
    };
    match format {
        DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => {}
        other => bail!("framebuffer pixel format {other:?} is not XRGB8888/ARGB8888"),
    }
    let buffer = buffer.ok_or_else(|| {
        anyhow!(
            "the kernel returned no GEM handle for framebuffer {fb:?}: this process lacks CAP_SYS_ADMIN \
             (run `sudo setcap cap_sys_admin+ep <path-to-server>` or use `server setup`)"
        )
    })?;
    let len = pitch as usize * height as usize + offset;

    // Preferred: export as dma-buf and map the dma-buf.
    let prime = card
        .buffer_to_prime_fd(buffer, libc::O_CLOEXEC as u32)
        .context("PRIME_HANDLE_TO_FD");
    let result = match prime {
        Ok(fd) => {
            let r = unsafe { memmap2::MmapOptions::new().len(len).map(fd.as_raw_fd()) }
                .context("mmap of PRIME dma-buf");
            r.map(|mmap| (mmap, "prime"))
        }
        Err(e) => Err(e),
    };
    let (mmap, via) = match result {
        Ok(ok) => ok,
        Err(prime_err) => {
            // Fallback: ask the driver for a fake mmap offset on the card node.
            tracing::debug!("PRIME mapping failed ({prime_err:#}), trying MAP_DUMB");
            let map = drm_ffi::mode::dumbbuffer::map(card.as_fd(), buffer.into(), 0, 0)
                .with_context(|| format!("MAP_DUMB failed after PRIME failed: {prime_err:#}"))?;
            let mmap = unsafe {
                memmap2::MmapOptions::new()
                    .offset(map.offset)
                    .len(len)
                    .map(card.as_fd().as_raw_fd())
            }
            .context("mmap of dumb buffer offset")?;
            (mmap, "map_dumb")
        }
    };
    // Our handle table reference is not needed any more: the mapping keeps the buffer alive.
    let _ = card.close_buffer(buffer);
    Ok(Mapping {
        fb,
        width,
        height,
        pitch,
        offset,
        format,
        mmap,
        via,
    })
}

/// Opens the card and waits until the display is active, retrying forever
/// (used by `run`) or until `deadline` (used by `capture --png`).
pub fn wait_for_display(
    configured_card: &str,
    connector: &str,
    deadline: Option<Duration>,
) -> Result<FrameSource> {
    let start = Instant::now();
    let mut last_log = None::<Instant>;
    loop {
        let attempt = find_card(configured_card, connector)
            .and_then(|card| locate_display(&card, connector).map(|d| (card, d)));
        match attempt {
            Ok((card, info)) => {
                tracing::info!(
                    "display {connector} on {} active: {}x{}@{} plane {:?}",
                    card.path().display(),
                    info.width,
                    info.height,
                    info.refresh,
                    info.plane
                );
                return Ok(FrameSource::new(card, info));
            }
            Err(e) => {
                if let Some(d) = deadline {
                    if start.elapsed() >= d {
                        return Err(e.context("display did not become active in time"));
                    }
                }
                if last_log
                    .map(|t| t.elapsed() > Duration::from_secs(10))
                    .unwrap_or(true)
                {
                    tracing::info!("waiting for display: {e:#}");
                    last_log = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

//! Support code for the end-to-end tests in `tests/`.
//!
//! Nothing here runs the real server or client binaries: the tests link the `server` and
//! `client` libraries and drive their media paths directly (synthetic frames instead of the
//! vkms framebuffer, an in-process SDP exchange instead of the HTTP signalling).

use std::sync::{Arc, Mutex};

/// Captures `log` records (the WebRTC stack logs through the `log` facade) so a test can
/// assert on what the libraries complained about.
#[derive(Default)]
pub struct LogCapture {
    records: Mutex<Vec<CapturedRecord>>,
    echo: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedRecord {
    pub level: log::Level,
    pub target: String,
    pub message: String,
}

impl LogCapture {
    /// Installs a capturing logger (once per process). `echo` also prints warnings to stderr,
    /// which `cargo test -- --nocapture` shows.
    pub fn install(echo: bool) -> Arc<LogCapture> {
        static INSTANCE: std::sync::OnceLock<Arc<LogCapture>> = std::sync::OnceLock::new();
        INSTANCE
            .get_or_init(|| {
                let capture = Arc::new(LogCapture {
                    records: Mutex::new(Vec::new()),
                    echo,
                });
                let logger: Box<dyn log::Log> = Box::new(CaptureLogger(Arc::clone(&capture)));
                if log::set_boxed_logger(logger).is_ok() {
                    log::set_max_level(log::LevelFilter::Info);
                }
                capture
            })
            .clone()
    }

    pub fn records(&self) -> Vec<CapturedRecord> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Records at `Warn` or above whose message contains `needle`.
    pub fn warnings_containing(&self, needle: &str) -> Vec<CapturedRecord> {
        self.records()
            .into_iter()
            .filter(|r| r.level <= log::Level::Warn && r.message.contains(needle))
            .collect()
    }

    pub fn clear(&self) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// The `log::Log` handle installed globally (newtype: `Arc<LogCapture>` is a foreign type).
struct CaptureLogger(Arc<LogCapture>);

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let captured = CapturedRecord {
            level: record.level(),
            target: record.target().to_string(),
            message: record.args().to_string(),
        };
        if self.0.echo && record.level() <= log::Level::Warn {
            eprintln!(
                "[{}] {}: {}",
                captured.level, captured.target, captured.message
            );
        }
        self.0
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(captured);
    }

    fn flush(&self) {}
}

/// NAL unit types found in an Annex B access unit.
pub fn nal_types(annexb: &[u8]) -> Vec<u8> {
    server::encoder::nal_units(annexb)
        .filter_map(server::encoder::nal_type)
        .collect()
}

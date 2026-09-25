//! Logging setup shared by the server and the client binaries.
//!
//! Our own code logs through `tracing`; the WebRTC stack (`webrtc`, `rtc`, `ice`, `dtls`,
//! `srtp`, ...) and FFmpeg log through the `log` facade. Library warnings come in bursts that
//! repeat the same line with a different number in it (`srtp ssrc=... index=N: duplicated`,
//! decoder errors for every damaged slice), so the `log` side goes through
//! [`RateLimitedLogger`]: per (target, level) at most [`BURST`] records are forwarded per
//! [`WINDOW`]; the rest are counted and summarised in one line when the window ends.
//!
//! [`init`] wires it all up: `tracing_subscriber::fmt` for output, an `EnvFilter` from
//! `RUST_LOG` or the verbosity flag, and the rate-limited bridge for `log` records.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Records allowed per (target, level) within one [`WINDOW`].
pub const BURST: u32 = 8;
/// Length of the rate-limiting window.
pub const WINDOW: Duration = Duration::from_secs(5);

struct Bucket {
    window_start: Instant,
    passed: u32,
    suppressed: u64,
    /// The first suppressed message, quoted in the summary line.
    example: String,
}

/// A `log::Log` wrapper that rate limits by (target, level).
///
/// Targets starting with one of `verbatim` prefixes (our own crates) are never limited.
pub struct RateLimitedLogger<L: log::Log> {
    inner: L,
    verbatim: Vec<&'static str>,
    buckets: Mutex<HashMap<(String, log::Level), Bucket>>,
    burst: u32,
    window: Duration,
}

impl<L: log::Log> RateLimitedLogger<L> {
    pub fn new(inner: L, verbatim: Vec<&'static str>) -> Self {
        Self::with_limits(inner, verbatim, BURST, WINDOW)
    }

    pub fn with_limits(
        inner: L,
        verbatim: Vec<&'static str>,
        burst: u32,
        window: Duration,
    ) -> Self {
        Self {
            inner,
            verbatim,
            buckets: Mutex::new(HashMap::new()),
            burst,
            window,
        }
    }

    /// Decides whether `record` passes and, when a window just ended with suppressed records,
    /// returns the summary that should be emitted before it.
    fn admit(&self, record: &log::Record, now: Instant) -> (bool, Option<String>) {
        let target = record.target();
        if self.verbatim.iter().any(|p| target.starts_with(p)) {
            return (true, None);
        }
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let key = (target.to_string(), record.level());
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            window_start: now,
            passed: 0,
            suppressed: 0,
            example: String::new(),
        });
        let mut summary = None;
        if now.duration_since(bucket.window_start) >= self.window {
            if bucket.suppressed > 0 {
                summary = Some(format!(
                    "{} more {} message(s) from {} suppressed in the last {:.0}s, e.g. {:?}",
                    bucket.suppressed,
                    record.level().as_str().to_ascii_lowercase(),
                    target,
                    now.duration_since(bucket.window_start).as_secs_f64(),
                    bucket.example
                ));
            }
            bucket.window_start = now;
            bucket.passed = 0;
            bucket.suppressed = 0;
            bucket.example.clear();
        }
        if bucket.passed < self.burst {
            bucket.passed += 1;
            (true, summary)
        } else {
            if bucket.suppressed == 0 {
                bucket.example = record.args().to_string();
            }
            bucket.suppressed += 1;
            (false, summary)
        }
    }
}

impl<L: log::Log> log::Log for RateLimitedLogger<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        let (pass, summary) = self.admit(record, Instant::now());
        if let Some(text) = summary {
            self.inner.log(
                &log::Record::builder()
                    .level(record.level())
                    .target(record.target())
                    .args(format_args!("{text}"))
                    .build(),
            );
        }
        if pass {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush()
    }
}

/// Default `RUST_LOG`-style filter for a verbosity level (`-v` count).
pub fn default_filter(verbose: u8) -> &'static str {
    match verbose {
        0 => "info,webrtc=warn,rtc=warn,ffmpeg=warn",
        1 => "debug,webrtc=info,rtc=info",
        _ => "trace",
    }
}

/// Installs the tracing subscriber (stderr, no targets) and the rate-limited `log` bridge.
///
/// `verbatim` lists target prefixes that are never rate limited (our own crates).
/// Idempotent: calling it twice keeps the first configuration.
pub fn init(verbose: u8, verbatim: Vec<&'static str>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter(verbose)));
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .try_init()
        .is_ok();
    if installed {
        // `try_init` does not install the `log` bridge when the subscriber is built by hand,
        // so `log` records only reach tracing through our rate-limited logger.
        let bridge = RateLimitedLogger::new(tracing_log::LogTracer::new(), verbatim);
        if log::set_boxed_logger(Box::new(bridge)).is_ok() {
            log::set_max_level(if verbose >= 2 {
                log::LevelFilter::Trace
            } else if verbose == 1 {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct Sink(Mutex<Vec<String>>);

    /// Newtype so the test logger can be shared with the assertions (orphan rule).
    struct SharedSink(Arc<Sink>);

    impl log::Log for SharedSink {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            self.0
                 .0
                .lock()
                .unwrap()
                .push(format!("{} {}", record.target(), record.args()));
        }
        fn flush(&self) {}
    }

    /// `admit` for a warn-level record (built and consumed in one expression: a `Record`
    /// borrows its `format_args!`).
    fn admit(
        limiter: &RateLimitedLogger<SharedSink>,
        target: &str,
        msg: &str,
        now: Instant,
    ) -> (bool, Option<String>) {
        limiter.admit(
            &log::Record::builder()
                .level(log::Level::Warn)
                .target(target)
                .args(format_args!("{}", msg))
                .build(),
            now,
        )
    }

    fn emit(limiter: &RateLimitedLogger<SharedSink>, target: &str, msg: &str) {
        log::Log::log(
            limiter,
            &log::Record::builder()
                .level(log::Level::Warn)
                .target(target)
                .args(format_args!("{}", msg))
                .build(),
        );
    }

    #[test]
    fn burst_passes_then_suppresses_and_summarises() {
        let sink = Arc::new(Sink::default());
        let limiter = RateLimitedLogger::with_limits(
            SharedSink(Arc::clone(&sink)),
            vec!["server"],
            3,
            Duration::from_secs(5),
        );
        let t0 = Instant::now();
        let mut passed = 0;
        for i in 0..10 {
            let msg = format!("srtp index={i}: duplicated");
            let (pass, summary) = admit(&limiter, "rtc::srtp", &msg, t0);
            assert!(summary.is_none());
            if pass {
                passed += 1;
            }
        }
        assert_eq!(passed, 3);
        // Our own targets are never limited.
        for _ in 0..20 {
            let (pass, _) = admit(&limiter, "server::pipeline", "x", t0);
            assert!(pass);
        }
        // Next window: one summary naming the count and an example, then records pass again.
        let (pass, summary) = admit(&limiter, "rtc::srtp", "later", t0 + Duration::from_secs(6));
        assert!(pass);
        let summary = summary.expect("summary after a window with suppressed records");
        assert!(
            summary.contains("7 more warn message(s) from rtc::srtp"),
            "{summary}"
        );
        assert!(summary.contains("index=3: duplicated"), "{summary}");
        // A quiet window produces no summary.
        let (_, summary) = admit(&limiter, "rtc::srtp", "quiet", t0 + Duration::from_secs(12));
        assert!(summary.is_none());
    }

    #[test]
    fn logger_forwards_summary_then_record() {
        let sink = Arc::new(Sink::default());
        let limiter = RateLimitedLogger::with_limits(
            SharedSink(Arc::clone(&sink)),
            vec![],
            1,
            Duration::from_millis(1),
        );
        emit(&limiter, "webrtc", "first");
        emit(&limiter, "webrtc", "second");
        std::thread::sleep(Duration::from_millis(5));
        emit(&limiter, "webrtc", "third");
        let lines = sink.0.lock().unwrap().clone();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "webrtc first");
        assert!(lines[1].starts_with("webrtc 1 more warn message(s) from webrtc"));
        assert_eq!(lines[2], "webrtc third");
    }

    #[test]
    fn default_filters_quiet_the_libraries_by_default() {
        assert!(default_filter(0).contains("rtc=warn"));
        assert!(default_filter(1).contains("rtc=info"));
        assert_eq!(default_filter(5), "trace");
    }
}

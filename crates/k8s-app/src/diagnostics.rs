//! GPUI profiler support for hang detection, frame statistics, and the debug overlay.
//! Set `K8S_GPUI_FRAME_STATS=1` to log frame statistics.

use std::{
    sync::{Mutex, PoisonError},
    thread,
    time::Duration,
};

use gpui::{
    App, AsyncApp, KeyBinding, Window, actions,
    profiler::hang::{HangDetector, HangIncident},
    profiler::journal::ForegroundEvent,
};

actions!(k8s_diagnostics, [CycleFrameOverlay]);

/// Interval for collecting hang data.
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
/// Interval for frame statistics logs.
const FRAME_STATS_INTERVAL: Duration = Duration::from_secs(5);
/// Maximum contributors listed per incident.
const MAX_CONTRIBUTORS: usize = 8;
/// Startup delay before hang detection begins.
const STARTUP_GRACE: Duration = Duration::from_millis(200);

pub fn init(cx: &mut App) {
    bind_overlay_key(cx);
    start_hang_detection(cx);
    if std::env::var_os("K8S_GPUI_FRAME_STATS").is_some() {
        start_frame_stats(cx);
    }
}

/// Cycles the frame overlay through its display modes.
fn bind_overlay_key(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("f1", CycleFrameOverlay, None)]);
    cx.on_action(|_: &CycleFrameOverlay, cx: &mut App| {
        for window in cx.windows() {
            let _ = window.update(cx, |_, window, _| {
                let next = window.debug_frame_overlay_mode().next();
                window.set_debug_frame_overlay_mode(next);
            });
        }
    });
}

fn start_hang_detection(cx: &App) {
    let hang_threshold = if cfg!(debug_assertions) {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(100)
    };
    let frame_budget = if cfg!(debug_assertions) {
        // Release frames use a lower budget than debug frames.
        Duration::from_millis(100)
    } else {
        // This budget is about one dropped display frame.
        Duration::from_millis(24)
    };

    let detector = Mutex::new(HangDetector::new(
        cx.foreground_journal(),
        hang_threshold,
        frame_budget,
    ));

    let spawned = thread::Builder::new()
        .name("k8s-gpui-hang-detect".to_owned())
        .spawn(move || {
            thread::sleep(STARTUP_GRACE);
            loop {
                thread::sleep(MONITOR_INTERVAL);
                let incidents = detector
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .poll();
                for incident in incidents {
                    report_incident(&incident, hang_threshold);
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("k8s-gpui: failed to start hang detector thread: {error}");
    }
}

fn report_incident(incident: &HangIncident, threshold: Duration) {
    let snapshot = &incident.snapshot;
    eprintln!(
        "[hang] trigger={:?} foreground={:.1}ms busy={:.0}% events={}",
        incident.trigger,
        millis(snapshot.occupancy()),
        snapshot.busy_fraction() * 100.0,
        incident.contributors.len(),
    );

    for event in incident.contributors.iter().take(MAX_CONTRIBUTORS) {
        eprintln!("[hang]   - {}", describe_event(event));
    }

    let hidden = incident.contributors.len().saturating_sub(MAX_CONTRIBUTORS);
    if hidden > 0 {
        eprintln!("[hang]   ... {hidden} more events");
    }

    if matches!(
        incident.trigger,
        gpui::profiler::hang::HangTrigger::Threshold
    ) {
        eprintln!("[hang]   one foreground operation took at least {threshold:?}");
    }
}

fn describe_event(event: &ForegroundEvent) -> String {
    let duration = millis(event.duration());
    match event {
        ForegroundEvent::TaskPoll(timing) => format!(
            "{duration:.1}ms task poll at {}:{}",
            timing.location.file(),
            timing.location.line()
        ),
        ForegroundEvent::Action(timing) => format!("{duration:.1}ms action {}", timing.name),
        ForegroundEvent::Input(timing) => format!("{duration:.1}ms input {}", timing.kind),
        ForegroundEvent::Draw(_) => format!("{duration:.1}ms draw"),
        ForegroundEvent::Present(_) => format!("{duration:.1}ms present"),
        ForegroundEvent::SmallPolls(flush) => format!(
            "{duration:.1}ms small polls x{} total {:.1}ms",
            flush.summary.count,
            millis(flush.summary.total)
        ),
    }
}

/// Logs frame-time percentiles for each window.
/// Offscreen windows have no vsync, so present intervals are not meaningful.
fn start_frame_stats(cx: &mut App) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(FRAME_STATS_INTERVAL).await;
            cx.update(|cx: &mut App| {
                for window in cx.windows() {
                    let _ = window.update(cx, |_, window: &mut Window, _| {
                        log_window_frame_stats(window)
                    });
                }
            });
        }
    })
    .detach();
}

fn log_window_frame_stats(window: &Window) {
    let frames = window.frame_duration_snapshot();
    let draw = &frames.draw_duration_histogram;
    if draw.is_empty() {
        return;
    }

    eprintln!(
        "[frames] draw_ms p50={:.2} p95={:.2} p99={:.2} max_ms={:.2} count={}",
        millis_nanos(draw.value_at_quantile(0.50)),
        millis_nanos(draw.value_at_quantile(0.95)),
        millis_nanos(draw.value_at_quantile(0.99)),
        millis_nanos(draw.max()),
        draw.len(),
    );

    let dirty_to_present = &frames.dirty_to_present_histogram;
    if !dirty_to_present.is_empty() {
        eprintln!(
            "[frames] dirty_to_present_ms p50={:.2} p99={:.2}",
            millis_nanos(dirty_to_present.value_at_quantile(0.50)),
            millis_nanos(dirty_to_present.value_at_quantile(0.99)),
        );
    }

    let input = window.input_latency_snapshot();
    if !input.latency_histogram.is_empty() {
        eprintln!(
            "[frames] input_to_present_ms p50={:.2} p99={:.2} events_per_frame={:.2}",
            millis_nanos(input.latency_histogram.value_at_quantile(0.50)),
            millis_nanos(input.latency_histogram.value_at_quantile(0.99)),
            input.events_per_frame_histogram.mean(),
        );
    }
}

fn millis(duration: Duration) -> f64 {
    millis_nanos(duration.as_nanos() as u64)
}

fn millis_nanos(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}

pub mod crash {
    use std::{
        backtrace::Backtrace,
        fs::{self, File, OpenOptions},
        io::{self, Write},
        path::{Path, PathBuf},
        process,
        thread::{self, Thread},
        time::SystemTime,
    };

    use chrono::{DateTime, SecondsFormat};

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    const LOG_FILE: &str = "crash.log";
    const LOG_DIR_MODE: u32 = 0o700;
    const LOG_FILE_MODE: u32 = 0o600;
    const MAX_MESSAGE_BYTES: usize = 4_096;
    const ABSENT: &str = "-";
    const UNKNOWN: &str = "<unknown>";
    const NON_STRING_PAYLOAD: &str = "<non-string panic payload>";
    const TRUNCATED: &str = "…[truncated]";

    #[derive(Clone)]
    struct Session {
        id: String,
        pid: u32,
        sid: Option<u32>,
        log: Option<PathBuf>,
    }

    impl Session {
        fn start_record(&self, at: SystemTime) -> String {
            let stamp = timestamp(at);
            let pid = self.pid;
            let id = self.id.as_str();
            let sid = field(self.sid);
            format!(
                "[{stamp}] start pid={pid} session={id} sid={sid} version={} build={}\n",
                env!("CARGO_PKG_VERSION"),
                build(),
            )
        }

        fn panic_record(
            &self,
            at: SystemTime,
            thread: &Thread,
            message: &str,
            location: &str,
            backtrace: &str,
        ) -> String {
            let stamp = timestamp(at);
            let pid = self.pid;
            let id = self.id.as_str();
            let sid = field(self.sid);
            let thread_name = thread.name().unwrap_or(ABSENT);
            let thread_id = thread.id();
            let frames = backtrace.trim_end();
            format!(
                "[{stamp}] panic pid={pid} session={id} sid={sid} \
                 thread={thread_name} thread_id={thread_id:?} location={location}\n\
                 [{stamp}] message {message}\n\
                 [{stamp}] backtrace begin\n{frames}\n\
                 [{stamp}] backtrace end\n",
            )
        }
    }

    pub fn install() {
        let started = SystemTime::now();
        let pid = process::id();
        let session = Session {
            id: session_id(started, pid),
            pid,
            sid: posix_session_id(),
            log: log_path(),
        };

        if let Some(path) = session.log.as_deref()
            && let Err(error) = append(path, &session.start_record(started))
        {
            eprintln!("[crash] cannot write {}: {error}", path.display());
        }
        let log_field = session
            .log
            .as_deref()
            .map_or_else(|| ABSENT.to_owned(), |path| path.display().to_string());
        eprintln!(
            "[crash] session={} pid={} sid={} log={log_field}",
            session.id,
            session.pid,
            field(session.sid),
        );

        let previous = std::panic::take_hook();
        let logged = session.clone();
        std::panic::set_hook(Box::new(move |info| {
            let record = logged.panic_record(
                SystemTime::now(),
                &thread::current(),
                &message_line(info.payload_as_str()),
                &info
                    .location()
                    .map_or_else(|| UNKNOWN.to_owned(), |location| location.to_string()),
                &Backtrace::force_capture().to_string(),
            );
            if let Some(path) = logged.log.as_deref() {
                let _ = append(path, &record);
            }
            previous(info);
        }));
    }

    fn build() -> &'static str {
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    }

    fn session_id(start: SystemTime, pid: u32) -> String {
        match start.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(delta) => format!("{}.{pid}", delta.as_millis()),
            Err(_) => format!("unknown.{pid}"),
        }
    }

    fn posix_session_id() -> Option<u32> {
        let stat = fs::read_to_string("/proc/self/stat").ok()?;
        parse_posix_session_id(&stat)
    }

    fn parse_posix_session_id(stat: &str) -> Option<u32> {
        let (_, fields) = stat.rsplit_once(')')?;
        fields.split_ascii_whitespace().nth(3)?.parse().ok()
    }

    fn timestamp(at: SystemTime) -> String {
        at.duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|delta| {
                DateTime::from_timestamp(delta.as_secs() as i64, delta.subsec_nanos())
            })
            .map_or_else(
                || UNKNOWN.to_owned(),
                |time| time.to_rfc3339_opts(SecondsFormat::Millis, true),
            )
    }

    fn field(value: Option<u32>) -> String {
        value.map_or_else(|| ABSENT.to_owned(), |value| value.to_string())
    }

    fn message_line(payload: Option<&str>) -> String {
        let mut line = String::new();
        for character in payload.unwrap_or(NON_STRING_PAYLOAD).chars() {
            let escape = match character {
                '\n' => Some("\\n"),
                '\r' => Some("\\r"),
                _ => None,
            };
            let width = escape.map_or(character.len_utf8(), str::len);
            if line.len() + width > MAX_MESSAGE_BYTES {
                line.push_str(TRUNCATED);
                return line;
            }
            match escape {
                Some(escape) => line.push_str(escape),
                None => line.push(character),
            }
        }
        line
    }

    fn log_path() -> Option<PathBuf> {
        let state_dir = k8s_core::paths::state_dir()?;
        Some(log_path_in(&state_dir))
    }

    fn log_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join(LOG_FILE)
    }

    fn append(path: &Path, record: &str) -> io::Result<()> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "crash log path has no parent")
        })?;
        prepare_dir(dir)?;
        let mut file = open_private(path)?;
        file.write_all(record.as_bytes())
    }

    fn prepare_dir(dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        restrict(dir, LOG_DIR_MODE)
    }

    fn open_private(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(LOG_FILE_MODE);
        let file = options.open(path)?;
        restrict(path, LOG_FILE_MODE)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn restrict(path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    #[cfg(not(unix))]
    fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use super::*;

        fn at(millis: u64) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
        }

        fn session(log: Option<PathBuf>) -> Session {
            Session {
                id: "1700000000123.4321".to_owned(),
                pid: 4321,
                sid: Some(4200),
                log,
            }
        }

        #[test]
        fn the_start_record_names_the_run() {
            let record = session(None).start_record(at(1_700_000_000_123));

            assert_eq!(
                record,
                format!(
                    "[2023-11-14T22:13:20.123Z] start pid=4321 session=1700000000123.4321 \
                     sid=4200 version={} build={}\n",
                    env!("CARGO_PKG_VERSION"),
                    build(),
                )
            );
        }

        #[test]
        fn the_panic_record_carries_time_pid_session_thread_and_backtrace() {
            let record = session(None).panic_record(
                at(1_700_000_000_456),
                &thread::current(),
                "index out of bounds",
                "crates/k8s-app/src/main.rs:42:9",
                "   0: rust_begin_unwind\n   1: k8s_app::main\n",
            );
            let prefix = "[2023-11-14T22:13:20.456Z] panic pid=4321 \
                          session=1700000000123.4321 sid=4200 thread=";

            assert!(record.starts_with(prefix), "{record}");
            assert!(
                record.contains(&format!("thread_id={:?}", thread::current().id())),
                "{record}"
            );
            assert!(
                record.contains("location=crates/k8s-app/src/main.rs:42:9"),
                "{record}"
            );
            assert!(
                record.contains("[2023-11-14T22:13:20.456Z] message index out of bounds\n"),
                "{record}"
            );
            assert!(record.contains("   1: k8s_app::main\n"), "{record}");
            assert!(
                record.ends_with("[2023-11-14T22:13:20.456Z] backtrace end\n"),
                "{record}"
            );
        }

        #[test]
        fn an_unreadable_field_stays_greppable() {
            let session = Session {
                sid: None,
                ..session(None)
            };

            assert_eq!(field(session.sid), ABSENT);
            assert!(session.start_record(at(0)).contains("sid=-"));
        }

        #[test]
        fn a_session_id_names_the_start_time_and_the_process() {
            assert_eq!(
                session_id(at(1_700_000_000_123), 4321),
                "1700000000123.4321"
            );
            assert_eq!(
                session_id(SystemTime::UNIX_EPOCH - Duration::from_secs(60), 7),
                "unknown.7"
            );
        }

        #[test]
        fn a_timestamp_is_utc_with_millisecond_precision() {
            assert_eq!(timestamp(at(1_700_000_000_123)), "2023-11-14T22:13:20.123Z");
            assert_eq!(
                timestamp(SystemTime::UNIX_EPOCH),
                "1970-01-01T00:00:00.000Z"
            );
            assert_eq!(
                timestamp(SystemTime::UNIX_EPOCH - Duration::from_secs(60)),
                UNKNOWN
            );
        }

        #[test]
        fn the_posix_session_is_read_after_the_command_name() {
            assert_eq!(
                parse_posix_session_id("4321 (k8s-app) S 1 4321 4200 0 -1 4194560"),
                Some(4200)
            );
            assert_eq!(
                parse_posix_session_id("4321 (a b) (c d) S 1 2 3 4"),
                Some(3)
            );
            assert_eq!(parse_posix_session_id("4321 (k8s-app) S 1 2"), None);
            assert_eq!(parse_posix_session_id("4321 (k8s-app) S 1 2 x"), None);
            assert_eq!(parse_posix_session_id("4321 k8s-app S 1 2 3 4"), None);
            assert_eq!(parse_posix_session_id(""), None);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn the_running_process_reports_its_posix_session() {
            assert!(posix_session_id().is_some());
        }

        #[test]
        fn a_panic_message_stays_on_one_line_and_is_bounded() {
            let bound = MAX_MESSAGE_BYTES + TRUNCATED.len();

            assert_eq!(message_line(None), NON_STRING_PAYLOAD);
            assert_eq!(
                message_line(Some("first\nsecond\rthird")),
                "first\\nsecond\\rthird"
            );
            assert_eq!(message_line(Some("")), "");

            let long = message_line(Some(&"x".repeat(MAX_MESSAGE_BYTES * 2)));
            assert!(long.ends_with(TRUNCATED), "{long}");
            assert!(long.len() <= bound, "{}", long.len());

            let wide = message_line(Some(&"日".repeat(MAX_MESSAGE_BYTES)));
            assert!(wide.ends_with(TRUNCATED), "{wide}");
            assert!(wide.len() <= bound, "{}", wide.len());

            let escaped = message_line(Some(&"\n".repeat(MAX_MESSAGE_BYTES)));
            assert!(escaped.ends_with(TRUNCATED), "{escaped}");
            assert!(!escaped.contains('\n'), "{escaped}");
        }

        #[test]
        fn the_crash_log_is_named_for_the_app_state_directory() {
            let state_dir = Path::new("/home/dev/.local/state/k8s-gpui");

            assert_eq!(log_path_in(state_dir), state_dir.join("crash.log"));
            assert!(log_path().is_none_or(|path| path.ends_with("k8s-gpui/crash.log")));
        }

        #[test]
        fn records_accumulate_in_one_crash_log() {
            let dir = tempfile::tempdir().expect("temp state dir");
            let path = log_path_in(&dir.path().join("k8s-gpui"));
            let session = session(Some(path.clone()));

            append(&path, &session.start_record(at(1_700_000_000_123)))
                .expect("append start record");
            append(
                &path,
                &session.panic_record(
                    at(1_700_000_000_456),
                    &thread::current(),
                    "boom",
                    "crates/k8s-app/src/main.rs:42:9",
                    "   0: frame",
                ),
            )
            .expect("append panic record");

            let log = fs::read_to_string(&path).expect("read crash log");
            assert!(log.contains("start pid=4321"), "{log}");
            assert!(log.contains("panic pid=4321"), "{log}");
            assert!(log.find("start pid=") < log.find("panic pid="), "{log}");
        }

        #[cfg(unix)]
        #[test]
        fn the_crash_log_and_its_directory_are_private() {
            let dir = tempfile::tempdir().expect("temp state dir");
            let log_dir = dir.path().join("k8s-gpui");
            let path = log_path_in(&log_dir);

            append(&path, "record\n").expect("append record");

            assert_eq!(mode(&log_dir), 0o700);
            assert_eq!(mode(&path), 0o600);
        }

        #[cfg(unix)]
        #[test]
        fn an_existing_readable_crash_log_is_tightened() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().expect("temp state dir");
            let path = log_path_in(&dir.path().join("k8s-gpui"));
            prepare_dir(path.parent().expect("log directory")).expect("prepare directory");
            fs::write(&path, "older record\n").expect("seed crash log");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen mode");

            append(&path, "newer record\n").expect("append record");

            assert_eq!(mode(&path), 0o600);
            let log = fs::read_to_string(&path).expect("read crash log");
            assert_eq!(log.lines().count(), 2);
        }

        #[cfg(unix)]
        fn mode(path: &Path) -> u32 {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(path).expect("stat path").permissions().mode() & 0o777
        }
    }
}

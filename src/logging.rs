//! Opt-in diagnostic log.
//!
//! Off unless `--log-file PATH` (or `VVMUX_LOG_FILE`) is given. The client owns the terminal in raw
//! mode and the session server is a detached daemon with no stderr, so the only place a log can go
//! is a file. The server is started with the same path, so one file records both sides of a
//! transient failure, each line tagged with its process role and pid.
//!
//! The file is created owner-only: log lines carry paths and session names.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use log::{LevelFilter, Log, Metadata, Record};

/// Verbosity of the diagnostic log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::Error,
            Self::Warn => LevelFilter::Warn,
            Self::Info => LevelFilter::Info,
            Self::Debug => LevelFilter::Debug,
            Self::Trace => LevelFilter::Trace,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

struct FileLogger {
    role: &'static str,
    pid: u32,
    file: Mutex<File>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // One write per line so concurrent client and server appends do not interleave mid-line.
        let line = format!(
            "{} {} pid={} {:<5} {}: {}\n",
            timestamp(),
            self.role,
            self.pid,
            record.level(),
            record.target(),
            record.args()
        );
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = file.write_all(line.as_bytes());
    }

    fn flush(&self) {
        let _ = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush();
    }
}

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

/// What was enabled, kept so the server launcher can hand the same settings to the daemon.
static ACTIVE: OnceLock<(PathBuf, LogLevel)> = OnceLock::new();

/// Start logging when `--log-file` or `VVMUX_LOG_FILE` asks for it; `level` likewise falls back to
/// `VVMUX_LOG_LEVEL`, then debug. Failure to open the file is reported on stderr and never stops
/// vvmux: a diagnostic aid must not become a new way to fail to launch.
pub fn init(path: Option<&Path>, level: Option<LogLevel>, role: &'static str) {
    let from_env = std::env::var_os("VVMUX_LOG_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let path = path.map(Path::to_path_buf).or(from_env);
    let level = level
        .or_else(|| {
            let value = std::env::var("VVMUX_LOG_LEVEL").ok()?;
            LogLevel::from_str(&value, true).ok()
        })
        .unwrap_or(LogLevel::Debug);
    let path = path.as_deref();
    let Some(path) = path else {
        return;
    };
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let file = match open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("vvmux: cannot open log file {}: {error}", path.display());
            return;
        }
    };
    let logger = LOGGER.get_or_init(|| FileLogger {
        role,
        pid: std::process::id(),
        file: Mutex::new(file),
    });
    if log::set_logger(logger).is_err() {
        return;
    }
    log::set_max_level(level.filter());
    let _ = ACTIVE.set((path, level));
    install_panic_hook();
    log::info!(
        "logging started: vvmux {} level={}",
        env!("CARGO_PKG_VERSION"),
        level.name()
    );
}

/// Arguments that make a spawned session server log to the same file at the same level.
pub fn server_args() -> Vec<OsString> {
    match ACTIVE.get() {
        Some((path, level)) => vec![
            "--log-file".into(),
            path.clone().into_os_string(),
            "--log-level".into(),
            level.name().into(),
        ],
        None => Vec::new(),
    }
}

fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Record panics, which in the detached server would otherwise vanish with its stderr.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        log::logger().flush();
        previous(info);
    }));
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, UTC, without a date-time dependency.
fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let in_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60,
        elapsed.subsec_millis()
    )
}

/// Howard Hinnant's days-since-epoch to proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_epochs() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_732), (2026, 10, 6));
    }

    #[test]
    fn no_server_args_when_logging_is_off() {
        assert!(server_args().is_empty());
    }
}

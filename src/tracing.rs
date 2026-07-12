//! Minimal, dependency-free tracing controlled by `RUST_LOG`.

#[derive(PartialEq, PartialOrd, Clone, Copy)]
pub enum Level {
    Off,
    Debug,
    Trace,
}

pub(crate) fn enabled(level: Level) -> bool {
    static LEVEL: std::sync::OnceLock<Level> = std::sync::OnceLock::new();
    let cfg_level = *LEVEL.get_or_init(|| match std::env::var("RUST_LOG").as_deref() {
        Ok("trace") => Level::Trace,
        Ok("debug") => Level::Debug,
        _ => Level::Off,
    });

    level <= cfg_level
}

macro_rules! log_debug {
    ($fmt:literal, $($arg:tt)*) => {
        if $crate::tracing::enabled($crate::tracing::Level::Debug) {
            eprintln!(concat!("\x1b[0;34mDEBUG:\x1b[0m ", $fmt), $($arg)*);
        }
    };

    ($fmt:literal) => {
        if $crate::tracing::enabled($crate::tracing::Level::Debug) {
            eprintln!(concat!("\x1b[0;34mDEBUG:\x1b[0m ", $fmt));
        }
    };
}

macro_rules! log_trace {
    ($fmt:literal, $($arg:tt)*) => {
        if $crate::tracing::enabled($crate::tracing::Level::Trace) {
            eprintln!(concat!("\x1b[0;35mTRACE:\x1b[0m ", $fmt), $($arg)*);
        }
    };

    ($fmt:literal) => {
        if $crate::tracing::enabled($crate::tracing::Level::Trace) {
            eprintln!(concat!("\x1b[0;35mTRACE:\x1b[0m ", $fmt));
        }
    };
}

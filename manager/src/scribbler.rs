//! # Scribbler — Structured Logging for Runpy
//!
//! A simple, environment-aware logging service with beautiful formatted output.
//!
//! ## Environment Variables
//!
//! - `ENVIRONMENT`: When set to `"development"`, enables maximum verbosity (bypass mode)
//! - `LOG`: Controls log level when not in development mode:
//!   - `"0"` or `"off"`: Logging disabled
//!   - `"1"` or `"error"`: Errors only
//!   - `"2"` or `"warning"`: Errors and warnings
//!   - `"3"` or `"info"`: Errors, warnings, and info (default)
//!   - `"4"` or `"debug"`: Include debug messages
//!   - `"5"` or `"verbose"`: Maximum verbosity
//!
//! ## Usage
//!
//! ```ignore
//! use runpy::Scribbler;
//!
//! let log = Scribbler::new();
//! log.info("Worker started");
//! log.debug("Connection established");
//! log.error("Failed to connect");
//! ```

use chrono::Local;
use std::env;
use std::sync::OnceLock;

// ── ANSI Color Codes ───────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

// ── Log Levels ─────────────────────────────────────────────────────────

/// Log severity levels, ordered from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Off = 0,
    Error = 1,
    Warning = 2,
    Info = 3,
    Debug = 4,
    Verbose = 5,
}

impl LogLevel {
    /// Parse a log level from an environment variable value.
    fn from_env(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "0" | "off" | "none" | "false" => LogLevel::Off,
            "1" | "error" | "err" => LogLevel::Error,
            "2" | "warning" | "warn" => LogLevel::Warning,
            "3" | "info" | "true" => LogLevel::Info,
            "4" | "debug" => LogLevel::Debug,
            "5" | "verbose" | "trace" | "all" => LogLevel::Verbose,
            _ => LogLevel::Info, // Default
        }
    }

    /// Get the display prefix for this level.
    fn prefix(&self) -> &'static str {
        match self {
            LogLevel::Off => "",
            LogLevel::Error => "ERROR",
            LogLevel::Warning => "WARN ",
            LogLevel::Info => "INFO ",
            LogLevel::Debug => "DEBUG",
            LogLevel::Verbose => "TRACE",
        }
    }

    /// Get the color code for this level.
    fn color(&self) -> &'static str {
        match self {
            LogLevel::Off => "",
            LogLevel::Error => RED,
            LogLevel::Warning => YELLOW,
            LogLevel::Info => GREEN,
            LogLevel::Debug => BLUE,
            LogLevel::Verbose => MAGENTA,
        }
    }
}

// ── Global Scribbler Instance ──────────────────────────────────────────

static GLOBAL_SCRIBBLER: OnceLock<Scribbler> = OnceLock::new();

/// Get the global scribbler instance.
pub fn scribbler() -> &'static Scribbler {
    GLOBAL_SCRIBBLER.get_or_init(Scribbler::new)
}

// ── Scribbler ──────────────────────────────────────────────────────────

/// The main logging service for runpy.
///
/// Provides structured, colorful logging with environment-based configuration.
#[derive(Debug, Clone)]
pub struct Scribbler {
    /// Maximum log level to display
    level: LogLevel,
    /// Whether we're in development mode (bypass)
    is_dev: bool,
    /// Whether to use colors (disabled if NO_COLOR is set)
    use_colors: bool,
}

impl Default for Scribbler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scribbler {
    /// Create a new Scribbler, reading configuration from environment variables.
    pub fn new() -> Self {
        let environment = env::var("ENVIRONMENT").unwrap_or_default();
        let is_dev = environment.eq_ignore_ascii_case("development")
            || environment.eq_ignore_ascii_case("dev");

        // In development mode, always use maximum verbosity
        let level = if is_dev {
            LogLevel::Verbose
        } else {
            let log_var = env::var("LOG").unwrap_or_else(|_| "info".to_string());
            LogLevel::from_env(&log_var)
        };

        // Respect NO_COLOR convention
        let use_colors = env::var("NO_COLOR").is_err();

        Self {
            level,
            is_dev,
            use_colors,
        }
    }

    /// Create a Scribbler with a specific log level (useful for testing).
    pub fn with_level(level: LogLevel) -> Self {
        Self {
            level,
            is_dev: false,
            use_colors: env::var("NO_COLOR").is_err(),
        }
    }

    /// Check if a given level should be logged.
    #[inline]
    fn should_log(&self, level: LogLevel) -> bool {
        level <= self.level
    }

    /// Format and print a log message.
    fn log(&self, level: LogLevel, component: Option<&str>, message: &str) {
        if !self.should_log(level) {
            return;
        }

        let timestamp = Local::now().format("%H:%M:%S%.3f");
        let prefix = level.prefix();

        if self.use_colors {
            let color = level.color();
            let component_str = component
                .map(|c| format!("{CYAN}{}{RESET} ", c))
                .unwrap_or_default();

            eprintln!(
                "{DIM}{}{RESET} {BOLD}{}{}{RESET} {component_str}{}",
                timestamp, color, prefix, message
            );
        } else {
            let component_str = component.map(|c| format!("[{}] ", c)).unwrap_or_default();
            eprintln!("{} {} {}{}", timestamp, prefix, component_str, message);
        }
    }

    // ── Public API ─────────────────────────────────────────────────────

    /// Log an error message (always visible unless logging is off).
    pub fn error(&self, message: &str) {
        self.log(LogLevel::Error, None, message);
    }

    /// Log an error with a component tag.
    pub fn error_with(&self, component: &str, message: &str) {
        self.log(LogLevel::Error, Some(component), message);
    }

    /// Log a warning message.
    pub fn warning(&self, message: &str) {
        self.log(LogLevel::Warning, None, message);
    }

    /// Log a warning with a component tag.
    pub fn warning_with(&self, component: &str, message: &str) {
        self.log(LogLevel::Warning, Some(component), message);
    }

    /// Log an informational message.
    pub fn info(&self, message: &str) {
        self.log(LogLevel::Info, None, message);
    }

    /// Log an info message with a component tag.
    pub fn info_with(&self, component: &str, message: &str) {
        self.log(LogLevel::Info, Some(component), message);
    }

    /// Log a debug message (only visible in debug/verbose mode).
    pub fn debug(&self, message: &str) {
        self.log(LogLevel::Debug, None, message);
    }

    /// Log a debug message with a component tag.
    pub fn debug_with(&self, component: &str, message: &str) {
        self.log(LogLevel::Debug, Some(component), message);
    }

    /// Log a verbose/trace message (only in verbose mode).
    pub fn verbose(&self, message: &str) {
        self.log(LogLevel::Verbose, None, message);
    }

    /// Log a verbose message with a component tag.
    pub fn verbose_with(&self, component: &str, message: &str) {
        self.log(LogLevel::Verbose, Some(component), message);
    }

    // ── Convenience methods for common patterns ────────────────────────

    /// Log a successful operation.
    pub fn success(&self, message: &str) {
        if self.should_log(LogLevel::Info) {
            let timestamp = Local::now().format("%H:%M:%S%.3f");
            if self.use_colors {
                eprintln!("{DIM}{}{RESET} {BOLD}{GREEN}  ✓  {RESET} {}", timestamp, message);
            } else {
                eprintln!("{} [OK] {}", timestamp, message);
            }
        }
    }

    /// Log a step/progress indicator.
    pub fn step(&self, step: u32, message: &str) {
        if self.should_log(LogLevel::Info) {
            let timestamp = Local::now().format("%H:%M:%S%.3f");
            if self.use_colors {
                eprintln!(
                    "{DIM}{}{RESET} {BOLD}{CYAN}[{:>2}]{RESET} {}",
                    timestamp, step, message
                );
            } else {
                eprintln!("{} [{:>2}] {}", timestamp, step, message);
            }
        }
    }

    /// Log a separator line for visual grouping.
    pub fn separator(&self) {
        if self.should_log(LogLevel::Info) {
            if self.use_colors {
                eprintln!("{DIM}────────────────────────────────────────{RESET}");
            } else {
                eprintln!("----------------------------------------");
            }
        }
    }

    // ── State inspection ───────────────────────────────────────────────

    /// Returns true if running in development mode.
    pub fn is_development(&self) -> bool {
        self.is_dev
    }

    /// Returns the current log level.
    pub fn level(&self) -> LogLevel {
        self.level
    }
}

// ── Convenience macros ─────────────────────────────────────────────────

/// Quick access to the global scribbler for logging.
/// 
/// Usage:
/// ```ignore
/// use runpy::log;
/// log!(info, "Server started on port {}", 8080);
/// log!(error, "Connection failed: {}", err);
/// log!(debug, Manager, "Worker count: {}", count);
/// ```
#[macro_export]
macro_rules! log {
    // With component: log!(info, Component, "message {}", arg)
    ($level:ident, $component:ident, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        $crate::scribbler::scribbler().$level(&format!("[{}] {}", stringify!($component), msg));
    }};
    // Without component: log!(info, "message {}", arg)
    ($level:ident, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        $crate::scribbler::scribbler().$level(&msg);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_ordering() {
        assert!(LogLevel::Error < LogLevel::Warning);
        assert!(LogLevel::Warning < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Verbose);
    }

    #[test]
    fn log_level_from_env_numeric() {
        assert_eq!(LogLevel::from_env("0"), LogLevel::Off);
        assert_eq!(LogLevel::from_env("1"), LogLevel::Error);
        assert_eq!(LogLevel::from_env("2"), LogLevel::Warning);
        assert_eq!(LogLevel::from_env("3"), LogLevel::Info);
        assert_eq!(LogLevel::from_env("4"), LogLevel::Debug);
        assert_eq!(LogLevel::from_env("5"), LogLevel::Verbose);
    }

    #[test]
    fn log_level_from_env_string() {
        assert_eq!(LogLevel::from_env("off"), LogLevel::Off);
        assert_eq!(LogLevel::from_env("error"), LogLevel::Error);
        assert_eq!(LogLevel::from_env("warning"), LogLevel::Warning);
        assert_eq!(LogLevel::from_env("info"), LogLevel::Info);
        assert_eq!(LogLevel::from_env("debug"), LogLevel::Debug);
        assert_eq!(LogLevel::from_env("verbose"), LogLevel::Verbose);
    }

    #[test]
    fn log_level_from_env_case_insensitive() {
        assert_eq!(LogLevel::from_env("ERROR"), LogLevel::Error);
        assert_eq!(LogLevel::from_env("Warning"), LogLevel::Warning);
        assert_eq!(LogLevel::from_env("INFO"), LogLevel::Info);
    }

    #[test]
    fn log_level_from_env_aliases() {
        assert_eq!(LogLevel::from_env("err"), LogLevel::Error);
        assert_eq!(LogLevel::from_env("warn"), LogLevel::Warning);
        assert_eq!(LogLevel::from_env("trace"), LogLevel::Verbose);
        assert_eq!(LogLevel::from_env("all"), LogLevel::Verbose);
    }

    #[test]
    fn log_level_from_env_default() {
        assert_eq!(LogLevel::from_env("unknown"), LogLevel::Info);
        assert_eq!(LogLevel::from_env(""), LogLevel::Info);
    }

    #[test]
    fn scribbler_with_level() {
        let scrib = Scribbler::with_level(LogLevel::Debug);
        assert_eq!(scrib.level(), LogLevel::Debug);
        assert!(!scrib.is_development());
    }

    #[test]
    fn should_log_respects_level() {
        let scrib = Scribbler::with_level(LogLevel::Warning);
        assert!(scrib.should_log(LogLevel::Error));
        assert!(scrib.should_log(LogLevel::Warning));
        assert!(!scrib.should_log(LogLevel::Info));
        assert!(!scrib.should_log(LogLevel::Debug));
    }
}

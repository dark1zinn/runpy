//! Unit tests for the Scribbler logging service.

use runpy::scribbler::{LogLevel, Scribbler};

#[test]
fn log_level_ordering() {
    assert!(LogLevel::Error < LogLevel::Warning);
    assert!(LogLevel::Warning < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Verbose);
}

#[test]
fn scribbler_with_level_sets_level() {
    let scrib = Scribbler::with_level(LogLevel::Debug);
    assert_eq!(scrib.level(), LogLevel::Debug);
}

#[test]
fn scribbler_with_level_not_dev() {
    let scrib = Scribbler::with_level(LogLevel::Debug);
    assert!(!scrib.is_development());
}

#[test]
fn scribbler_default_equals_new() {
    // Both should use env vars, so they should produce equivalent results
    let scrib1 = Scribbler::new();
    let scrib2 = Scribbler::default();
    assert_eq!(scrib1.level(), scrib2.level());
    assert_eq!(scrib1.is_development(), scrib2.is_development());
}

#[test]
fn log_level_off_is_lowest() {
    assert!(LogLevel::Off < LogLevel::Error);
}

#[test]
fn log_level_verbose_is_highest() {
    assert!(LogLevel::Verbose > LogLevel::Debug);
    assert!(LogLevel::Verbose > LogLevel::Info);
    assert!(LogLevel::Verbose > LogLevel::Warning);
    assert!(LogLevel::Verbose > LogLevel::Error);
}

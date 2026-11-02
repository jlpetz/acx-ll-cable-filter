//! The process's one ONNX Runtime environment. ort rc.15 refuses a second `EnvironmentBuilder::build`, even after the first
//! environment is dropped, so it is built once, here, and kept: parallel tests and every engine restart share it.
//!
//! Telemetry is off. ONNX Runtime's log goes to the `log` crate (target `onnxruntime`). With a custom logger ONNX Runtime
//! starts at Verbose, so the level is set to Warning at once; sessions log at Error (`session.rs`). Logging happens while
//! sessions are built, on the worker before it is ready, not per run.

use std::sync::{Arc, OnceLock};

use ort::environment::Environment;
use ort::logging::LogLevel;

static ENV: OnceLock<Result<Environment, String>> = OnceLock::new();

/// The environment, built on first use.
pub fn env() -> Result<&'static Environment, String> {
    ENV.get_or_init(|| {
        let logger = Arc::new(|level: LogLevel, category: &str, _id: &str, location: &str, message: &str| {
            let l = match level {
                LogLevel::Verbose => log::Level::Trace,
                LogLevel::Info => log::Level::Debug,
                LogLevel::Warning => log::Level::Warn,
                LogLevel::Error | LogLevel::Fatal => log::Level::Error,
            };
            log::log!(target: "onnxruntime", l, "[{category}] {message} ({location})");
        });
        let env = ort::init()
            .with_name("noise-filter")
            .with_telemetry(false)
            .with_logger(logger)
            .build()
            .map_err(|e| format!("ONNX Runtime's environment: {e}"))?;
        env.set_log_level(LogLevel::Warning);
        Ok(env)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// ONNX Runtime's build line ("ORT Build Info: git-branch=rel-1.31.0, ...").
pub fn build_info() -> &'static str {
    ort::info()
}

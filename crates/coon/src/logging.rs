use chrono::Utc;
use fern::Dispatch;
use log::LevelFilter;

/// Initialize file-based logging for the application
///
/// This sets up logging to write to a file in the logs directory with timestamps,
/// log levels, and proper formatting for debugging and monitoring.
pub fn init_file_logging(log_level: LevelFilter) -> Result<(), fern::InitError> {
    // Create logs directory if it doesn't exist
    std::fs::create_dir_all("logs").unwrap_or_else(|e| {
        eprintln!("Warning: Could not create logs directory: {}", e);
    });

    // Generate log filename with timestamp
    let log_filename = format!("logs/coon_{}.log", Utc::now().format("%Y%m%d_%H%M%S"));

    Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}:{}] {}",
                Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                message
            ))
        })
        .level(log_level)
        .chain(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_filename)?,
        )
        .apply()?;

    // Log the initialization
    log::info!("Logging initialized. Log file: {}", log_filename);
    log::info!("Log level set to: {:?}", log_level);

    Ok(())
}

/// Initialize logging with different levels based on environment or user preference
pub fn init_logging() -> Result<(), fern::InitError> {
    // Check environment variable for log level, default to Info
    let log_level = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info".to_string())
        .parse::<LevelFilter>()
        .unwrap_or(LevelFilter::Info);

    init_file_logging(log_level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_initialization() {
        // Test that logging can be initialized without errors
        let result = init_file_logging(LevelFilter::Debug);
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_file_creation() {
        // Initialize logging
        init_file_logging(LevelFilter::Info).unwrap();

        // Log a test message
        log::info!("Test log message");

        // Check that logs directory exists
        assert!(std::path::Path::new("logs").exists());
    }
}

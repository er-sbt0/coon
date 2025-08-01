# Logging Configuration

The application now uses file-based logging instead of printing to stdout/stderr.

## Log Files

- **Location**: `logs/` directory in the project root
- **Format**: `logs/coon_YYYYMMDD_HHMMSS.log`
- **Content**: Timestamped log entries with file/line information

## Log Levels

The application supports different log levels via the `RUST_LOG` environment variable:

- `error` - Only error messages
- `warn` - Warnings and errors
- `info` - Informational messages, warnings, and errors (default)
- `debug` - All messages including debug information
- `trace` - Most verbose logging

## Usage Examples

```bash
# Run with default info level logging
./target/release/coon

# Run with debug logging
RUST_LOG=debug ./target/release/coon

# Run with error-only logging
RUST_LOG=error ./target/release/coon /path/to/project
```

## Log Format

Each log entry includes:
- Timestamp (YYYY-MM-DD HH:MM:SS.mmm)
- Log level (INFO, DEBUG, WARN, ERROR)
- Source file and line number
- Log message

Example:
```
2025-08-01 09:53:11.859[INFO][src/main.rs:22] No arguments provided. Running with demo data...
```

## Finding Log Files

The latest log file can be found programmatically using the `core_data::logging::get_latest_log_file()` function.

## Benefits

- **No stdout pollution**: Application output is clean
- **Debugging**: Full trace of application execution
- **Monitoring**: Ability to analyze application behavior
- **Error tracking**: Centralized error logging
- **Performance**: Log levels allow filtering verbose output in production

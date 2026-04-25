use clap::Parser;
use log::error;
use std::path::Path;
use tui::LayoutSettings;

mod demo;
mod logging;
mod runner;

#[derive(Parser)]
#[command(about = "COON - C/C++ call graph visualizer")]
struct Args {
    /// Path to the C/C++ project directory (omit to run with demo data)
    project_path: Option<String>,

    /// Start the web server instead of the TUI (e.g. --web 8080)
    #[arg(long, value_name = "PORT")]
    web: Option<u16>,

    /// Vertical gap between sibling nodes in rows
    #[arg(long, value_name = "ROWS")]
    node_separation: Option<f32>,

    /// Horizontal distance between graph levels in columns
    #[arg(long, value_name = "COLS")]
    level_separation: Option<f32>,

    /// Width of each node box in characters
    #[arg(long, value_name = "CHARS")]
    node_width: Option<f32>,

    /// Height of each node box in rows
    #[arg(long, value_name = "ROWS")]
    node_height: Option<f32>,

    /// Minimum gap between separate subtrees
    #[arg(long, value_name = "ROWS")]
    subtree_separation: Option<f32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init_logging().map_err(|e| format!("Failed to initialize logging: {}", e))?;

    let args = Args::parse();

    let defaults = LayoutSettings::default();
    let settings = LayoutSettings {
        node_separation: args.node_separation.unwrap_or(defaults.node_separation),
        level_separation: args.level_separation.unwrap_or(defaults.level_separation),
        node_width: args.node_width.unwrap_or(defaults.node_width),
        node_height: args.node_height.unwrap_or(defaults.node_height),
        subtree_separation: args
            .subtree_separation
            .unwrap_or(defaults.subtree_separation),
    };

    match (args.web, args.project_path) {
        (Some(port), None) => runner::run_web(port).await?,
        (Some(port), Some(ref path)) => {
            if Path::new(path).exists() {
                runner::run_web_with_lsp(path, port).await?;
            } else {
                error!("Error: Project path '{}' does not exist", path);
                std::process::exit(1);
            }
        }
        (None, None) => runner::run_with_demo_data(settings).await?,
        (None, Some(ref path)) => {
            if Path::new(path).exists() {
                runner::run_with_lsp(path, settings).await?;
            } else {
                error!("Error: Project path '{}' does not exist", path);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

use log::error;
use std::env;
use std::path::Path;

mod logging;
mod runner;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init_logging().map_err(|e| format!("Failed to initialize logging: {}", e))?;

    let args: Vec<String> = env::args().collect();

    match args.len() {
        1 => runner::run_with_demo_data().await?,
        2 => {
            let project_path = &args[1];
            if Path::new(project_path).exists() {
                runner::run_with_lsp(project_path).await?;
            } else {
                error!("Error: Project path '{}' does not exist", project_path);
                print_usage();
                std::process::exit(1);
            }
        }
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_usage() {
    let program_name = env::args().next().unwrap_or_else(|| "coon".to_string());
    println!("Usage:");
    println!("  {} [project_path]", program_name);
    println!();
    println!("Options:");
    println!("  project_path    Path to the project directory for LSP analysis");
    println!("                  If not provided, runs with demo data");
    println!();
    println!("Examples:");
    println!("  {}                    # Run with demo data", program_name);
    println!(
        "  {} /path/to/project   # Analyze a real project",
        program_name
    );
}

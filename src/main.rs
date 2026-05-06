use clap::Parser;
use rdc::cli::{run, Cli};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

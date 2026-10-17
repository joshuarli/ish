#[path = "semantic_bridge/mod.rs"]
mod semantic_bridge;

fn main() {
    if let Err(error) = semantic_bridge::run_from_args(std::env::args().skip(1)) {
        eprintln!("semantic bridge: {error}");
        std::process::exit(1);
    }
}

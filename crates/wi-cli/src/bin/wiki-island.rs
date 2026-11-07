#![forbid(unsafe_code)]

#[path = "../main.rs"]
mod cli;

fn main() -> anyhow::Result<()> {
    cli::main()
}

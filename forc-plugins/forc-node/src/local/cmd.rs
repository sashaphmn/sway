use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
pub struct LocalCmd {
    chain_config: Option<PathBuf>,
}

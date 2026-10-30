//! Replays a devnet block log and checks every state hash: from genesis, or,
//! for a log rotated after a state snapshot, from that snapshot (whose own
//! hash is checked first).

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    log: PathBuf,
    /// Re-execute from genesis even if a snapshot exists (needs a log kept
    /// from genesis: `hc-devnet --full-log`).
    #[arg(long)]
    from_genesis: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let loaded = hc_node::node::load_chain(&args.log, !args.from_genesis, |_, _, _, _| {}).map_err(|e| anyhow::anyhow!("replay failed: {e}"))?;
    let l = loaded.ok_or_else(|| anyhow::anyhow!("{} is missing or empty", args.log.display()))?;
    let from = if l.start_height == 0 { "genesis".to_string() } else { format!("snapshot at height {}", l.start_height) };
    println!("replay OK from {from}: height={} state_hash={:?}", l.state.height, l.state.hash());
    Ok(())
}

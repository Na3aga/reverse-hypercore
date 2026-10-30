//! `hc-core`: a deterministic, clean-room HyperCore-like execution engine.
//!
//! The engine is a pure function `(State, Block) -> (State', BlockOutput)`:
//! integer fixed-point math only, ordered containers only, no clocks or
//! randomness. The same genesis and block sequence therefore always yield the
//! same [`exchange::State::hash`].
//!
//! Behaviour comes from public Hyperliquid documentation and black-box
//! observation, with structure cross-checked against strings and type metadata
//! in the hl-node binary. No decompiled code was used. See
//! `docs/FINDINGS.md` at the repository root.

pub mod action;
pub mod book;
pub mod clearinghouse;
pub mod events;
pub mod evm_bridge;
pub mod exchange;
pub mod fixed;
pub mod funding;
pub mod oracle;
pub mod risk;
pub mod tpsl;
pub mod twap;
pub mod vault;
pub mod types;

pub use action::{Action, BlockBody, SignedTx};
pub use exchange::{BlockOutput, Genesis, Params, State};

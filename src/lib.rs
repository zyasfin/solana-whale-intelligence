//! Signal Forge — modular monolith core domain scaffold.
//!
//! This crate is being evolved in-place from the pre-freeze research service
//! (`solana-whale-intelligence`) toward the frozen `PLAN SWI` final architecture
//! (canonical artifact: `PLAN-SWI-final-architecture-2026-08-29.md`).
//!
//! Architecture principle #12: modular monolith + workers + isolated signer;
//! no premature microservices. The `sf` module tree mirrors the frozen domain
//! boundaries, not the old flat CLI modules.

pub mod sf;

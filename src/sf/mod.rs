//! Signal Forge core domain modules (modular monolith scaffold).
//!
//! Each submodule corresponds to a frozen PLAN SWI domain boundary.

pub mod core;
pub mod decision;
pub mod graph;
pub mod auth;
pub mod identity;
pub mod intent;
pub mod jobs;
pub mod source;
pub mod ingest;
pub mod portfolio;
pub mod token;
pub mod wallet;
pub mod dashboard;
pub mod browser;
pub mod caller;
pub mod narrative;
pub mod autonomy;
pub mod execution;
pub mod lp;
pub mod revival;
pub mod strategy;
pub mod cost_basis;
pub mod ingest_runtime;
pub mod signal_gate;
pub mod wallet_runtime;
pub mod portfolio_runtime;
pub mod source_health_runtime;
pub mod token_runtime;
pub mod caller_runtime;
pub mod revival_runtime;
pub mod narrative_runtime;
pub mod dashboard_runtime;
pub mod graph_runtime;
pub mod lp_runtime;
pub mod strategy_runtime;

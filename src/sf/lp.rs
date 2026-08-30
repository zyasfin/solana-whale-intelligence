//! LP pool/wallet intelligence (Phase 4).
//!
//! Canonical source: PLAN SWI §8.9 (LP Pool Intelligence) and §8.10 (LP Wallet
//! Intelligence). LP products: Solana Meteora DLMM + Robinhood Uniswap/Pancake.
//! Ethereum/Base/BSC LP = N/A (doc §1 frozen scope).

use serde::{Deserialize, Serialize};

/// LP protocol (doc §1 + §8.9). Unsupported protocols return `N/A` (gate #12).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LpProtocol {
    MeteoraDlmm,
    UniswapV2,
    UniswapV3,
    UniswapV4,
    PancakeV2,
    PancakeV3,
}

/// LP pool intelligence (doc §8.9). PnL components remain separate (gate #13).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LpPoolIntelligence {
    pub chain: String,
    pub protocol: LpProtocol,
    pub pool_address: String,
    pub active_bin: Option<String>, // Meteora
    pub bin_step: Option<String>,
    pub range: Option<String>,
    pub tvl: Option<String>,
    pub active_tvl: Option<String>,
    pub reserves: Option<serde_json::Value>,
    pub volume: Option<String>,
    pub fees: Option<String>,
    pub fee_to_tvl: Option<f64>,
}

/// LP PnL components (doc "LP accounting"): inventory/fee/reward/IL/swap friction/
/// gas/rent/tips/realized/unrealized remain SEPARATE (gate #13).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LpPnl {
    pub inventory_pnl: Option<String>,
    pub fee_pnl: Option<String>,
    pub reward_pnl: Option<String>,
    pub impermanent_loss_estimate: Option<String>,
    pub swap_rebalance_friction: Option<String>,
    pub gas_rent_tips: Option<String>,
    pub realized_pnl: Option<String>,
    pub unrealized_pnl: Option<String>,
}

/// LP wallet intelligence (doc §8.10): position timing, range choice, reseed
/// behavior, fee/inventory/reward PnL, outcome concentration, regime consistency,
/// pool specialization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LpWalletIntelligence {
    pub wallet: String,
    pub position_timing: Option<String>,
    pub hold_duration: Option<String>,
    pub range_choice: Option<String>,
    pub reseed_behavior: Option<String>,
    pub outcome_concentration: Option<String>,
    pub regime_consistency: Option<String>,
    pub pool_specialization: Option<Vec<String>>,
    pub pnl: LpPnl,
}

/// Range chamber state (dashboard surface §22 + LP Range Chamber).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LpRangeChamber {
    pub pool_address: String,
    pub current_range: Option<String>,
    pub recommended_range: Option<String>,
    pub range_shift_reason: Option<String>,
}

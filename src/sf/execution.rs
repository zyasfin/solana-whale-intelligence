//! Secure execution domain (Phase 6).
//!
//! Canonical source: PLAN SWI §14 (Token Auto-Trader), §15 (LP Autopilot),
//! §16 (Execution state machine), §17 (Signer policy), §18 (Key custody),
//! §19 (Kill switches). Execution is bounded and signer-validated.

use serde::{Deserialize, Serialize};

/// Token auto-trader actions (doc §14). Arbitrary transfers/calldata/wallet
/// sweep are NOT trading actions.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradeAction {
    Buy,
    Sell,
    PartialSell,
    Close,
    EmergencyExit,
}

/// LP autopilot actions (doc §15). CREATE_POOL/CREATE_TOKEN are separate,
/// disabled, later capabilities (doc §1 LP semantics).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LpAction {
    OpenPosition,
    AddLiquidity,
    ClaimFees,
    CompoundFees,
    PartialWithdraw,
    ClosePosition,
    ReseedPosition,
    SwapResiduals,
    EmergencyExit,
}

/// Canonical closed action (REV-011-F04): either a token trade or an LP action.
/// This is the authoritative action type for intent/decision — a free-form
/// `String` is no longer accepted. `Trade::EmergencyExit` and `Lp::EmergencyExit`
/// are DISTINCT and unambiguous.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Trade(TradeAction),
    Lp(LpAction),
}

/// Policy limits (doc §14 "Policy limits"): max per trade/token/chain/strategy,
/// exposure, rate windows, slippage/impact/gas/tip, depth, daily loss/drawdown,
/// reserve, allowlists, cooldown/denylist.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyLimits {
    pub max_per_trade: Option<String>,
    pub max_per_token: Option<String>,
    pub max_per_chain: Option<String>,
    pub max_per_strategy: Option<String>,
    pub max_total_exposure: Option<String>,
    pub trades_per_window: Option<u32>,
    pub notional_per_window: Option<String>,
    pub max_slippage: Option<f64>,
    pub max_price_impact: Option<f64>,
    pub max_gas_tip: Option<String>,
    pub max_daily_loss: Option<String>,
    pub max_drawdown: Option<String>,
    pub wallet_reserve: Option<String>,
    pub allowed_routers: Vec<String>,
    pub allowed_programs: Vec<String>,
    pub allowed_contracts: Vec<String>,
    pub cooldown: Option<String>,
    pub denylist: Vec<String>,
}

/// Kill switch (doc §19): global/per-chain/per-wallet/per-strategy/per-protocol/
/// source-health/daily-loss/signer-local. When halted: no new entries/signatures
/// by default; reconciliation continues; EXIT_ONLY permits risk-reducing actions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KillSwitch {
    pub scope: KillSwitchScope,
    pub scope_key: Option<String>,
    pub mode: KillSwitchMode,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KillSwitchScope {
    Global,
    Chain,
    Wallet,
    Strategy,
    Protocol,
    SourceHealth,
    DailyLoss,
    SignerLocal,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KillSwitchMode {
    Halt,
    ExitOnly,
}

/// Signer policy checks (doc §17): the signer independently validates the full
/// transaction semantics before signing. This is a closed checklist, not a
/// generic "sign anything" endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignerPolicy {
    pub chain_id: Option<String>,
    pub chain_genesis: Option<String>,
    pub policy_active: bool,
    pub policy_not_expired: bool,
    pub policy_not_halted: bool,
    pub intent_hash_valid: bool,
    pub nonce_valid: bool,
    pub router_allowed: bool,
    pub program_allowed: bool,
    pub function_selector: Option<String>,
    pub token_pair_verified: bool,
    pub recipient_verified: bool,
    pub max_native_debit: Option<String>,
    pub max_token_debit: Option<String>,
    pub min_output: Option<String>,
    pub slippage_ok: bool,
    pub price_impact_ok: bool,
    pub deadline_ok: bool,
    pub simulation_delta_ok: bool,
    // REV-011-F02: remaining PLAN §17 mandatory semantics.
    pub workspace_binding_valid: bool,
    pub wallet_binding_valid: bool,
    pub policy_binding_valid: bool,
    pub idempotency_binding_valid: bool,
    pub factory_allowed: bool,
    pub manager_allowed: bool,
    pub pool_verified: bool,
    pub authority_verified: bool,
    pub gas_ok: bool,
    pub priority_fee_ok: bool,
    pub tip_ok: bool,
    pub rent_ok: bool,
    pub writable_accounts_allowed: bool,
    pub approvals_bounded: bool,
    pub instructions_decoded: bool,
    pub no_unrelated_operations: bool,
}

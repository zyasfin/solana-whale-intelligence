//! Auth/audit logic scaffold: OIDC, RBAC, WebAuthn step-up, audit sink.
//!
//! Canonical source: PLAN SWI §5 "Network and identity" (246-276), §10
//! "Access/control", and acceptance gates #8, #9, #10, #17.
//!
//! This module defines the *interfaces* and *pure logic* for the final auth
//! model. The concrete OIDC/WebAuthn transports are wired in a later Phase 0
//! task (requires runtime dependencies); these types are dependency-free so the
//! scaffold stays buildable.

use serde::{Deserialize, Serialize};

use super::identity::{Identity, Role, WorkloadIdentity};

/// Result of an authorization check (RBAC enforcement).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthzDecision {
    Allow,
    Deny(String), // reason
}

/// An audit record. Secrets are redacted before write (gate #17); only
/// fingerprints/status are recorded. Append-only at the storage layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    pub actor: Actor,
    pub action: String,
    pub entity_type: String,
    pub entity_id: String,
    pub before_state: Option<serde_json::Value>, // redacted
    pub after_state: Option<serde_json::Value>,  // redacted
}

/// Who performed an action: a human (OIDC identity) or a worker (workload
/// identity). This encodes gate #10 (worker uses workload identity, not human
/// session).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Actor {
    Human { identity: Identity },
    Worker { workload: WorkloadIdentity },
}

/// The authorization interface every protected operation goes through.
/// Implementations enforce RBAC and, for privileged changes, require WebAuthn
/// step-up (gate #9).
pub trait Authorizer {
    /// Authorize `actor` to perform `action` under the given role.
    fn authorize(&self, actor: &Actor, role: Role, action: &str) -> AuthzDecision;

    /// Whether the current request requires a WebAuthn step-up before a
    /// privileged change is applied (gate #9).
    fn requires_step_up(&self, action: &str) -> bool;
}

/// The append-only audit sink (gate #17). Writes are immutable; secrets are
/// redacted before persistence.
pub trait AuditSink {
    fn append(&mut self, record: AuditRecord) -> Result<(), AuditError>;
}

#[derive(Debug)]
pub enum AuditError {
    Rejected(String),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditError::Rejected(msg) => write!(f, "audit write rejected: {msg}"),
        }
    }
}

impl std::error::Error for AuditError {}

/// A default `Authorizer` that denies by default (fail-closed, principle #7).
/// Concrete RBAC policies attach to specific actions in a later task.
pub struct DenyByDefaultAuthorizer;

impl Authorizer for DenyByDefaultAuthorizer {
    fn authorize(&self, _actor: &Actor, _role: Role, _action: &str) -> AuthzDecision {
        // Fail closed: no implicit allow until an explicit policy is wired.
        AuthzDecision::Deny("no explicit RBAC policy matched (fail-closed)".into())
    }

    fn requires_step_up(&self, action: &str) -> bool {
        // Privileged changes require WebAuthn step-up (gate #9). Until the
        // concrete list is frozen, treat privileged actions as requiring it.
        matches!(
            action,
            "policy.activate" | "signer.configure" | "workspace.delete" | "identity.rotate"
        )
    }
}

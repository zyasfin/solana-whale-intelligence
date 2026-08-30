//! Identity domain: OIDC identity, workload identity, RBAC.
//!
//! Canonical source: PLAN SWI §5 "Network and identity" (lines 246-276) and
//! §10 "Access/control".

use serde::{Deserialize, Serialize};

/// Permanent identity is `issuer + subject` (doc line 252). Email is a mutable
/// display claim, NOT an identity key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,  // OIDC issuer URL, e.g. https://accounts.google.com
    pub subject: String, // OIDC `sub` claim
}

impl Identity {
    pub fn new(issuer: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
        }
    }

    /// The permanent identity key is the (issuer, subject) pair.
    pub fn key(&self) -> (String, String) {
        (self.issuer.clone(), self.subject.clone())
    }
}

/// Workload identity for workers (doc line 255; gate #10). Workers authenticate
/// via workload identity, never a human session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadIdentity {
    pub name: String,
    pub key_fingerprint: String,
    pub scopes: Vec<String>,
}

/// RBAC role. The frozen architecture requires role-based access control
/// (gate #9). The exact role enum is a frozen-decision candidate; a minimal
/// closed set is provided for the scaffold.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Operator,
    Analyst,
}

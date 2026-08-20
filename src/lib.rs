//! Fail a build when a query plan gets worse.
//!
//! **The rule the whole thing is built on:** never fail a build for a plan
//! change that cannot be shown to be worse. See `regress`.
//!
//! The logic lives here rather than in the binary so integration tests can
//! drive it against a real Postgres — which they must, because a plan is the
//! database's opinion and no model of one is worth gating on.

pub mod baseline;
pub mod explain;
pub mod fingerprint;
pub mod queries;
pub mod regress;
pub mod shape;

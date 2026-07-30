//! The analysis layer: graph build, effect inference, policy, detectors.

pub mod baseline;
pub mod build;
pub mod check;
pub mod compress;
pub mod detect;
pub mod diff;
pub mod effects;
pub mod entity_id;
pub mod envelope;
pub mod findings;
pub mod fix;
pub mod gate;
pub mod harness;
pub mod health;
pub mod infer;
pub mod inline;
pub mod install;
pub mod naming;
pub mod policy;
pub mod precheck;
pub mod query;
pub mod rename;
pub mod skeleton;
pub mod source;
pub mod suppress;
pub mod tier3;

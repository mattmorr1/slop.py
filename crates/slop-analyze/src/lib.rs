//! The analysis layer: graph build, effect inference, policy, detectors.

pub mod baseline;
pub mod build;
pub mod check;
pub mod compress;
pub mod config;
pub mod context;
pub mod coverage;
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
pub mod index;
pub mod infer;
pub mod inline;
pub mod naming;
pub mod policy;
pub mod precheck;
pub mod prewrite;
pub mod query;
pub mod rename;
pub mod repair;
pub mod retrieve;
pub mod scan;
pub mod search;
pub mod skeleton;
pub mod snapshot;
pub mod source;
pub mod split;
pub mod suppress;
pub mod tier3;
pub mod world;

#[cfg(test)]
mod test_support;

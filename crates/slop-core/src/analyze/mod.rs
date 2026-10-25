//! The analysis layer: graph build, effect inference, policy, detectors.

pub mod baseline;
pub mod build;
pub mod check;
pub mod config;
pub mod coverage;
pub mod detect;
pub mod diff;
pub mod effects;
pub mod entity_id;
pub mod findings;
pub mod gate;
pub mod harness;
pub mod health;
pub mod index;
pub mod missing;
pub mod infer;
pub mod overlay;
pub mod policy;
pub mod precheck;
pub mod prewrite;
pub mod query;
pub mod refresh;
pub mod retrieve;
pub mod scan;
pub mod search;
pub mod snapshot;
pub mod source;
pub mod suppress;
pub mod world;

#[cfg(test)]
mod test_support;

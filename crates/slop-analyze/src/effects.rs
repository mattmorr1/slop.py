//! Coarse effect inference (D3): a hand-curated seed table of primitive
//! effect sources, direct effect acquisition via references to them, and
//! transitive propagation up the Calls graph to a fixpoint.
//!
//! Seeds match on *dotted entity-id prefixes* (`requests`,
//! `pathlib.Path.write_text`), not bare module prefixes — symbol-level
//! granularity is what keeps `requests.models.Response.json` (reading data
//! you already have) from counting as fresh network I/O.

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;

pub struct Seed {
    /// Dotted path prefix matched against an external entity's ID.
    pub prefix: &'static str,
    pub effect: Effect,
}

/// M2 seed table: stdlib + the most common libraries, deliberately coarse.
pub const SEEDS: &[Seed] = &[
    // Net
    Seed { prefix: "urllib.request", effect: Effect::Net },
    Seed { prefix: "urllib3", effect: Effect::Net },
    Seed { prefix: "requests", effect: Effect::Net },
    Seed { prefix: "socket", effect: Effect::Net },
    Seed { prefix: "http.client", effect: Effect::Net },
    Seed { prefix: "httpx", effect: Effect::Net },
    Seed { prefix: "aiohttp", effect: Effect::Net },
    Seed { prefix: "websockets", effect: Effect::Net },
    Seed { prefix: "smtplib", effect: Effect::Net },
    Seed { prefix: "ftplib", effect: Effect::Net },
    Seed { prefix: "paramiko", effect: Effect::Net },
    Seed { prefix: "grpc", effect: Effect::Net },
    Seed { prefix: "botocore", effect: Effect::Net },
    Seed { prefix: "boto3", effect: Effect::Net },
    // FS — open() is read-or-write depending on mode we can't see: both.
    Seed { prefix: "open", effect: Effect::FsRead },
    Seed { prefix: "open", effect: Effect::FsWrite },
    Seed { prefix: "builtins.open", effect: Effect::FsRead },
    Seed { prefix: "builtins.open", effect: Effect::FsWrite },
    Seed { prefix: "io.open", effect: Effect::FsRead },
    Seed { prefix: "io.open", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.read_text", effect: Effect::FsRead },
    Seed { prefix: "pathlib.Path.read_bytes", effect: Effect::FsRead },
    Seed { prefix: "pathlib.Path.open", effect: Effect::FsRead },
    Seed { prefix: "pathlib.Path.write_text", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.write_bytes", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.unlink", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.mkdir", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.rmdir", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.touch", effect: Effect::FsWrite },
    Seed { prefix: "pathlib.Path.rename", effect: Effect::FsWrite },
    Seed { prefix: "shutil", effect: Effect::FsWrite },
    Seed { prefix: "tempfile", effect: Effect::FsWrite },
    Seed { prefix: "os.remove", effect: Effect::FsWrite },
    Seed { prefix: "os.unlink", effect: Effect::FsWrite },
    Seed { prefix: "os.rename", effect: Effect::FsWrite },
    Seed { prefix: "os.makedirs", effect: Effect::FsWrite },
    Seed { prefix: "os.mkdir", effect: Effect::FsWrite },
    Seed { prefix: "os.rmdir", effect: Effect::FsWrite },
    Seed { prefix: "os.listdir", effect: Effect::FsRead },
    Seed { prefix: "os.walk", effect: Effect::FsRead },
    Seed { prefix: "os.stat", effect: Effect::FsRead },
    Seed { prefix: "os.path.exists", effect: Effect::FsRead },
    Seed { prefix: "json.load", effect: Effect::FsRead },
    Seed { prefix: "json.dump", effect: Effect::FsWrite },
    // DB
    Seed { prefix: "sqlite3", effect: Effect::Db },
    Seed { prefix: "psycopg2", effect: Effect::Db },
    Seed { prefix: "psycopg", effect: Effect::Db },
    Seed { prefix: "pymysql", effect: Effect::Db },
    Seed { prefix: "mysql", effect: Effect::Db },
    Seed { prefix: "sqlalchemy", effect: Effect::Db },
    Seed { prefix: "redis", effect: Effect::Db },
    Seed { prefix: "pymongo", effect: Effect::Db },
    Seed { prefix: "asyncpg", effect: Effect::Db },
    // Env
    Seed { prefix: "os.environ", effect: Effect::Env },
    Seed { prefix: "os.getenv", effect: Effect::Env },
    Seed { prefix: "os.putenv", effect: Effect::Env },
    Seed { prefix: "dotenv", effect: Effect::Env },
    // Nondeterminism
    Seed { prefix: "random", effect: Effect::Nondeterminism },
    Seed { prefix: "secrets", effect: Effect::Nondeterminism },
    Seed { prefix: "uuid.uuid1", effect: Effect::Nondeterminism },
    Seed { prefix: "uuid.uuid4", effect: Effect::Nondeterminism },
    Seed { prefix: "time.time", effect: Effect::Nondeterminism },
    Seed { prefix: "time.monotonic", effect: Effect::Nondeterminism },
    Seed { prefix: "datetime.datetime.now", effect: Effect::Nondeterminism },
    Seed { prefix: "datetime.datetime.today", effect: Effect::Nondeterminism },
    Seed { prefix: "datetime.date.today", effect: Effect::Nondeterminism },
    // Concurrency
    Seed { prefix: "threading", effect: Effect::Concurrency },
    Seed { prefix: "multiprocessing", effect: Effect::Concurrency },
    Seed { prefix: "concurrent.futures", effect: Effect::Concurrency },
    Seed { prefix: "subprocess", effect: Effect::Concurrency },

    // --- JavaScript / Node (multi-language groundwork) ---
    // The graph/effect detectors run over any SCIP-indexed language; these
    // seed the JS/TS ecosystem. Prefixes are the expected scip-typescript
    // entity-id forms (module/package name first); validate against a real
    // `scip-typescript` index before relying on them in anger. No collisions
    // with the Python seeds above map to a *different* effect.
    // Net
    Seed { prefix: "https", effect: Effect::Net },
    Seed { prefix: "node:https", effect: Effect::Net },
    Seed { prefix: "node:http", effect: Effect::Net },
    Seed { prefix: "node:net", effect: Effect::Net },
    Seed { prefix: "node:dns", effect: Effect::Net },
    Seed { prefix: "node:tls", effect: Effect::Net },
    Seed { prefix: "axios", effect: Effect::Net },
    Seed { prefix: "node-fetch", effect: Effect::Net },
    Seed { prefix: "undici", effect: Effect::Net },
    Seed { prefix: "got", effect: Effect::Net },
    Seed { prefix: "superagent", effect: Effect::Net },
    Seed { prefix: "ws", effect: Effect::Net },
    // FS — the `fs` module and its promises API do both directions.
    Seed { prefix: "fs", effect: Effect::FsRead },
    Seed { prefix: "fs", effect: Effect::FsWrite },
    Seed { prefix: "node:fs", effect: Effect::FsRead },
    Seed { prefix: "node:fs", effect: Effect::FsWrite },
    Seed { prefix: "fs-extra", effect: Effect::FsWrite },
    // DB
    Seed { prefix: "pg", effect: Effect::Db },
    Seed { prefix: "mysql", effect: Effect::Db },
    Seed { prefix: "mysql2", effect: Effect::Db },
    Seed { prefix: "mongodb", effect: Effect::Db },
    Seed { prefix: "mongoose", effect: Effect::Db },
    Seed { prefix: "ioredis", effect: Effect::Db },
    Seed { prefix: "knex", effect: Effect::Db },
    Seed { prefix: "sequelize", effect: Effect::Db },
    Seed { prefix: "prisma", effect: Effect::Db },
    Seed { prefix: "@prisma/client", effect: Effect::Db },
    // Env
    Seed { prefix: "process.env", effect: Effect::Env },
    Seed { prefix: "dotenv", effect: Effect::Env },
    // Concurrency
    Seed { prefix: "child_process", effect: Effect::Concurrency },
    Seed { prefix: "node:child_process", effect: Effect::Concurrency },
    Seed { prefix: "worker_threads", effect: Effect::Concurrency },
    Seed { prefix: "node:worker_threads", effect: Effect::Concurrency },
    Seed { prefix: "cluster", effect: Effect::Concurrency },
];

/// Entity-id prefixes that never seed, even under a matching seed prefix:
/// data you already hold, not fresh effect acquisition.
pub const EXCLUDES: &[&str] = &[
    "requests.models.Response",
    "requests.exceptions",
    "requests.structures",
    "httpx.Response",
    "urllib3.exceptions",
    "sqlite3.Row",
    "random.Random.seed", // seeding a PRNG you own is deterministic setup
];

fn prefix_matches(dotted: &str, prefix: &str) -> bool {
    dotted == prefix || (dotted.starts_with(prefix) && dotted[prefix.len()..].starts_with('.'))
}

/// Effects a dotted external entity ID seeds, after exclusions.
pub fn seed_effects_for(dotted_id: &str) -> Vec<Effect> {
    if EXCLUDES.iter().any(|ex| prefix_matches(dotted_id, ex)) {
        return Vec::new();
    }
    let mut effects: Vec<Effect> = SEEDS
        .iter()
        .filter(|s| prefix_matches(dotted_id, s.prefix))
        .map(|s| s.effect)
        .collect();
    effects.sort();
    effects.dedup();
    effects
}

/// Mark external seed nodes as effect sources and add `HasEffect` edges from
/// every internal entity that references them. A `HasEffect` edge means
/// *direct* acquisition — the signal infra-bypass keys off. Then propagate
/// effects transitively along `Calls` edges to a fixpoint (cycles converge
/// because set-union is monotone).
pub fn infer_effects(built: &mut BuiltGraph) {
    // Direct acquisition.
    let mut direct: Vec<(NodeIndex, NodeIndex, Effect)> = Vec::new();
    for (source, entity) in built.graph.entities() {
        if entity.entity_type != NodeType::EffectSource {
            continue;
        }
        let dotted = entity.id.replace("::", ".");
        for effect in seed_effects_for(&dotted) {
            for edge in built.graph.graph.edges_directed(source, Direction::Incoming) {
                if *edge.weight() == EdgeKind::Calls {
                    direct.push((edge.source(), source, effect));
                }
            }
        }
    }
    for (user, source, effect) in direct {
        built.graph.graph[source].effect_signature.insert(effect);
        built.graph.graph[user].effect_signature.insert(effect);
        built.graph.add_edge(user, source, EdgeKind::HasEffect);
    }

    // Transitive propagation: caller absorbs callee effects.
    loop {
        let mut changed = false;
        let edges: Vec<(NodeIndex, NodeIndex)> = built
            .graph
            .graph
            .edge_indices()
            .filter(|&e| built.graph.graph[e] == EdgeKind::Calls)
            .filter_map(|e| built.graph.graph.edge_endpoints(e))
            .collect();
        for (caller, callee) in edges {
            let callee_effects = built.graph.graph[callee].effect_signature.clone();
            if built.graph.graph[caller]
                .effect_signature
                .union_with(&callee_effects)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_respect_boundaries_and_excludes() {
        assert_eq!(seed_effects_for("requests.api.post"), vec![Effect::Net]);
        assert!(seed_effects_for("requests.models.Response.json").is_empty());
        assert!(seed_effects_for("urllib.parse.urlencode").is_empty());
        assert!(seed_effects_for("requestsish.thing").is_empty());
        assert_eq!(
            seed_effects_for("pathlib.Path.write_text"),
            vec![Effect::FsWrite]
        );
        assert!(seed_effects_for("pathlib.Path.name").is_empty());
        assert_eq!(
            seed_effects_for("open"),
            vec![Effect::FsRead, Effect::FsWrite]
        );
    }

    #[test]
    fn node_js_seeds_resolve() {
        assert_eq!(seed_effects_for("axios.get"), vec![Effect::Net]);
        assert_eq!(seed_effects_for("pg.Client.query"), vec![Effect::Db]);
        assert_eq!(seed_effects_for("process.env.HOME"), vec![Effect::Env]);
        assert_eq!(
            seed_effects_for("fs.readFileSync"),
            vec![Effect::FsRead, Effect::FsWrite]
        );
        assert_eq!(
            seed_effects_for("node:child_process.exec"),
            vec![Effect::Concurrency]
        );
        // Boundary: a package that merely starts with a seed name doesn't match.
        assert!(seed_effects_for("axioms.thing").is_empty());
    }
}

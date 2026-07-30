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

    // --- JavaScript / Node ---
    // Matched against `external_effect_id` (entity_id.rs), which normalizes a
    // scip-typescript symbol to its *import identity* — the npm package name
    // (`axios`) or the node module specifier (`"node:fs"` -> `fs`), plus the
    // member. Validated against a real scip-typescript index (see the
    // `ts_probe` fixture): `axios.get`, `fs.readFileSync`, `process.env`, and
    // `child_process.exec` all resolve.
    // Net
    Seed { prefix: "http", effect: Effect::Net },
    Seed { prefix: "https", effect: Effect::Net },
    Seed { prefix: "net", effect: Effect::Net },
    Seed { prefix: "dns", effect: Effect::Net },
    Seed { prefix: "tls", effect: Effect::Net },
    Seed { prefix: "axios", effect: Effect::Net },
    Seed { prefix: "node-fetch", effect: Effect::Net },
    Seed { prefix: "undici", effect: Effect::Net },
    Seed { prefix: "got", effect: Effect::Net },
    Seed { prefix: "superagent", effect: Effect::Net },
    Seed { prefix: "ws", effect: Effect::Net },
    // FS — the `fs` module and its promises API do both directions.
    Seed { prefix: "fs", effect: Effect::FsRead },
    Seed { prefix: "fs", effect: Effect::FsWrite },
    Seed { prefix: "fs-extra", effect: Effect::FsWrite },
    // DB (need the package's @types to resolve, e.g. @types/pg)
    Seed { prefix: "pg", effect: Effect::Db },
    Seed { prefix: "mysql", effect: Effect::Db },
    Seed { prefix: "mysql2", effect: Effect::Db },
    Seed { prefix: "mongodb", effect: Effect::Db },
    Seed { prefix: "mongoose", effect: Effect::Db },
    Seed { prefix: "ioredis", effect: Effect::Db },
    Seed { prefix: "knex", effect: Effect::Db },
    Seed { prefix: "sequelize", effect: Effect::Db },
    Seed { prefix: "@prisma/client", effect: Effect::Db },
    // Env
    Seed { prefix: "process.env", effect: Effect::Env },
    Seed { prefix: "dotenv", effect: Effect::Env },
    // Concurrency
    Seed { prefix: "child_process", effect: Effect::Concurrency },
    Seed { prefix: "worker_threads", effect: Effect::Concurrency },
    Seed { prefix: "cluster", effect: Effect::Concurrency },

    // --- Rust ---
    // Rust IDs are crate-prefixed by `entity_id` (`std::env::var`), so these
    // never collide with the bare Node prefixes above (`net`, `fs`, `http`).
    // std's module layout maps almost 1:1 onto the effect lattice, which is why
    // this table is short. FS is enumerated per-member, not as a blanket
    // `std.fs`, so reads don't also report as writes.
    // Net
    Seed { prefix: "std.net", effect: Effect::Net },
    Seed { prefix: "tokio.net", effect: Effect::Net },
    Seed { prefix: "reqwest", effect: Effect::Net },
    Seed { prefix: "hyper", effect: Effect::Net },
    Seed { prefix: "ureq", effect: Effect::Net },
    Seed { prefix: "isahc", effect: Effect::Net },
    Seed { prefix: "tonic", effect: Effect::Net },
    Seed { prefix: "tungstenite", effect: Effect::Net },
    // FS read
    Seed { prefix: "std.fs.read", effect: Effect::FsRead },
    Seed { prefix: "std.fs.read_to_string", effect: Effect::FsRead },
    Seed { prefix: "std.fs.read_dir", effect: Effect::FsRead },
    Seed { prefix: "std.fs.metadata", effect: Effect::FsRead },
    Seed { prefix: "std.fs.canonicalize", effect: Effect::FsRead },
    Seed { prefix: "std.fs.File.open", effect: Effect::FsRead },
    Seed { prefix: "std.fs.DirEntry.metadata", effect: Effect::FsRead },
    // FS write
    Seed { prefix: "std.fs.write", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.copy", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.rename", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.create_dir", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.create_dir_all", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.remove_file", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.remove_dir", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.remove_dir_all", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.set_permissions", effect: Effect::FsWrite },
    Seed { prefix: "std.fs.File.create", effect: Effect::FsWrite },
    // OpenOptions is read-or-write by builder flags we can't see: both.
    Seed { prefix: "std.fs.OpenOptions", effect: Effect::FsRead },
    Seed { prefix: "std.fs.OpenOptions", effect: Effect::FsWrite },
    Seed { prefix: "tokio.fs", effect: Effect::FsRead },
    Seed { prefix: "tokio.fs", effect: Effect::FsWrite },
    // DB
    Seed { prefix: "sqlx", effect: Effect::Db },
    Seed { prefix: "diesel", effect: Effect::Db },
    Seed { prefix: "rusqlite", effect: Effect::Db },
    Seed { prefix: "postgres", effect: Effect::Db },
    Seed { prefix: "tokio_postgres", effect: Effect::Db },
    Seed { prefix: "sea_orm", effect: Effect::Db },
    // Env
    Seed { prefix: "std.env.var", effect: Effect::Env },
    Seed { prefix: "std.env.var_os", effect: Effect::Env },
    Seed { prefix: "std.env.vars", effect: Effect::Env },
    Seed { prefix: "std.env.vars_os", effect: Effect::Env },
    Seed { prefix: "std.env.set_var", effect: Effect::Env },
    Seed { prefix: "std.env.remove_var", effect: Effect::Env },
    Seed { prefix: "dotenvy", effect: Effect::Env },
    // Nondeterminism
    Seed { prefix: "rand", effect: Effect::Nondeterminism },
    Seed { prefix: "std.time.SystemTime.now", effect: Effect::Nondeterminism },
    Seed { prefix: "std.time.Instant.now", effect: Effect::Nondeterminism },
    Seed { prefix: "uuid.Uuid.new_v4", effect: Effect::Nondeterminism },
    Seed { prefix: "chrono.Utc.now", effect: Effect::Nondeterminism },
    Seed { prefix: "chrono.Local.now", effect: Effect::Nondeterminism },
    // Concurrency
    Seed { prefix: "std.process.Command", effect: Effect::Concurrency },
    Seed { prefix: "std.thread.spawn", effect: Effect::Concurrency },
    Seed { prefix: "std.sync.mpsc", effect: Effect::Concurrency },
    Seed { prefix: "tokio.spawn", effect: Effect::Concurrency },
    Seed { prefix: "tokio.task", effect: Effect::Concurrency },
    Seed { prefix: "rayon", effect: Effect::Concurrency },
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
        // Inputs are the normalized `external_effect_id` forms (see the
        // ts_probe integration test for the real end-to-end path).
        assert_eq!(seed_effects_for("axios.get"), vec![Effect::Net]);
        assert_eq!(seed_effects_for("pg.query"), vec![Effect::Db]);
        assert_eq!(seed_effects_for("process.env"), vec![Effect::Env]);
        assert_eq!(
            seed_effects_for("fs.readFileSync"),
            vec![Effect::FsRead, Effect::FsWrite]
        );
        assert_eq!(seed_effects_for("child_process.exec"), vec![Effect::Concurrency]);
        // Boundary: a package that merely starts with a seed name doesn't match.
        assert!(seed_effects_for("axioms.thing").is_empty());
    }

    #[test]
    fn rust_seeds_resolve() {
        assert_eq!(seed_effects_for("std.env.var"), vec![Effect::Env]);
        assert_eq!(seed_effects_for("std.fs.create_dir_all"), vec![Effect::FsWrite]);
        assert_eq!(seed_effects_for("std.net.TcpStream.connect"), vec![Effect::Net]);
        assert_eq!(seed_effects_for("std.process.Command.spawn"), vec![Effect::Concurrency]);
        assert_eq!(seed_effects_for("reqwest.blocking.Client.get"), vec![Effect::Net]);
        assert_eq!(
            seed_effects_for("std.fs.OpenOptions.open"),
            vec![Effect::FsRead, Effect::FsWrite]
        );
        // A read member must not also report as a write: `std.fs.read` is a
        // sibling of `std.fs.read_to_string`, not a prefix of it.
        assert_eq!(seed_effects_for("std.fs.read_to_string"), vec![Effect::FsRead]);
        assert_eq!(seed_effects_for("std.fs.read_dir"), vec![Effect::FsRead]);
        // Data you already hold is not fresh acquisition.
        assert!(seed_effects_for("std.fs.DirEntry.path").is_empty());
        assert!(seed_effects_for("std.time.Duration.as_secs").is_empty());
        // The bare Node prefixes must not swallow crate-prefixed Rust IDs.
        assert!(seed_effects_for("std.collections.hash.map.HashMap.insert").is_empty());
    }
}

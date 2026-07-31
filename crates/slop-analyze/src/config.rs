//! Config sprawl: one environment variable read directly from many places.
//!
//! This is what resource identity buys. The effect lattice already knows a
//! function reads the environment, and `infra-bypass` can already say "route
//! `Env` through the config module" when a policy names one. Neither can see
//! that `DATABASE_URL` is pulled out of `os.environ` in six unrelated modules,
//! because without the variable *name* every `Env` acquisition looks alike.
//!
//! It is a specific and recurring AI-slop shape: each session needs a setting,
//! each session reads it where it stands, and the config surface ends up with
//! no single definition and six subtly different defaults.
//!
//! Codebase-relative (D8): a variable read once or twice is just code. The
//! finding is a variable read in enough *distinct modules* that the codebase has
//! evidently lost track of where its configuration lives.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use slop_parse::{resources, Language};

use crate::findings::{Finding, Severity};
use crate::source::{is_test_file, FileFacts};

/// Distinct modules that must read one variable before it counts as sprawl.
/// Two places is a pair, which is how a setting and its default legitimately
/// look; four is a codebase that no longer knows where its config lives.
///
/// Deliberately *not* tuned down to fire on the dogfood corpus. The widest
/// spread there is 3 (`ANTHROPIC_API_KEY` in vigil, 12.6k Python files); every
/// other variable sits at 1-2. So this rule reports nothing on any repo we have,
/// and its precision is unproven — the validated half of resource identity is
/// the env surface in the world model, which is a fact rather than a judgement.
///
/// What the corpus *does* suggest is a different formulation: vigil reads
/// `POSTGRES_HOST`/`PORT`/`DB`/`USER`/`PASSWORD` in two modules each, which is
/// two modules independently assembling one Postgres config. That is a shared
/// *set* of variables rather than one over-read variable, and it would be the
/// next thing to detect here.
const MIN_SPREAD: usize = 4;

/// Where one variable is read: module label -> the `file:line` sites in it.
type Sites = BTreeMap<String, Vec<(String, u32)>>;

/// Every environment variable the repo reads, and where. Test files are
/// excluded: a test setting `TZ` or faking a key is doing its job.
fn env_surface(repo: &Path, facts: &[FileFacts]) -> BTreeMap<String, Sites> {
    let mut surface: BTreeMap<String, Sites> = BTreeMap::new();
    for ff in facts {
        if is_test_file(&ff.file) {
            continue;
        }
        let Some(lang) = Language::from_path(&ff.file) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(repo.join(&ff.file)) else {
            continue;
        };
        let module = crate::source::module_label(&ff.file);
        for read in resources::env_reads(lang, &source) {
            surface
                .entry(read.var)
                .or_default()
                .entry(module.clone())
                .or_default()
                .push((ff.file.clone(), read.line));
        }
    }
    surface
}

/// The variables this repo reads, most-spread first — a world-model fact worth
/// telling an agent before it invents a seventh way to read one.
pub fn env_vars(repo: &Path, facts: &[FileFacts]) -> Vec<(String, usize)> {
    let mut vars: Vec<(String, usize)> = env_surface(repo, facts)
        .into_iter()
        .map(|(var, sites)| (var, sites.len()))
        .collect();
    vars.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    vars
}

/// Config-sprawl: one variable read directly in [`MIN_SPREAD`] or more modules.
pub fn config_sprawl(repo: &Path, facts: &[FileFacts]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (var, sites) in env_surface(repo, facts) {
        if sites.len() < MIN_SPREAD {
            continue;
        }
        let modules: BTreeSet<&String> = sites.keys().collect();
        // Anchor on the first site in file order so the finding has one stable
        // home rather than one per read.
        let (file, line) = sites
            .values()
            .flatten()
            .min_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
            .expect("a site exists")
            .clone();
        let names = modules.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ");
        findings.push(Finding {
            rule: "config-sprawl",
            severity: Severity::Advisory,
            entity: format!("env::{var}"),
            file,
            lines: (line as usize, line as usize),
            message: format!(
                "`{var}` is read straight from the environment in {} modules: {names}",
                modules.len()
            ),
            fix_guidance: format!(
                "Read `{var}` once in a config module and have the other {} call sites take it from there — one definition, one default",
                sites.values().map(Vec::len).sum::<usize>().saturating_sub(1)
            ),
        });
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_parse::FunctionFacts;

    /// Writes `files` into a temp repo and returns (repo, facts). Facts only
    /// need to name the file — the reads come from the source on disk.
    fn repo_of(name: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, Vec<FileFacts>) {
        let dir = std::env::temp_dir().join(format!("slop-cfg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut facts = Vec::new();
        for (path, body) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, body).unwrap();
            facts.push(FileFacts { file: (*path).to_string(), functions: Vec::<FunctionFacts>::new() });
        }
        (dir, facts)
    }

    fn read_of(var: &str) -> String {
        format!("import os\nV = os.environ[\"{var}\"]\n")
    }

    #[test]
    fn one_variable_across_many_modules_is_sprawl() {
        let files: Vec<(String, String)> = (0..4)
            .map(|i| (format!("m{i}.py"), read_of("DATABASE_URL")))
            .collect();
        let refs: Vec<(&str, &str)> =
            files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (repo, facts) = repo_of("sprawl", &refs);
        let found = config_sprawl(&repo, &facts);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].entity, "env::DATABASE_URL");
        assert!(found[0].message.contains("4 modules"), "{}", found[0].message);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_setting_read_in_a_couple_of_places_is_just_code() {
        let (repo, facts) = repo_of(
            "pair",
            &[("a.py", &read_of("TZ")), ("b.py", &read_of("TZ"))],
        );
        assert!(config_sprawl(&repo, &facts).is_empty());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn many_reads_inside_one_module_are_centralized_already() {
        // Four reads, one module: that module *is* the config boundary.
        let body = (0..4).map(|_| read_of("API_KEY")).collect::<String>();
        let (repo, facts) = repo_of("central", &[("config.py", &body)]);
        assert!(config_sprawl(&repo, &facts).is_empty());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_files_do_not_count_toward_spread() {
        let mut files: Vec<(String, String)> = (0..3)
            .map(|i| (format!("tests/test_{i}.py"), read_of("API_KEY")))
            .collect();
        files.push(("app.py".into(), read_of("API_KEY")));
        let refs: Vec<(&str, &str)> =
            files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (repo, facts) = repo_of("tests", &refs);
        assert!(config_sprawl(&repo, &facts).is_empty());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn the_env_surface_ranks_by_how_far_a_variable_has_spread() {
        let (repo, facts) = repo_of(
            "surface",
            &[
                ("a.py", "import os\nA = os.environ[\"WIDE\"]\n"),
                ("b.py", "import os\nB = os.environ[\"WIDE\"]\n"),
                ("c.py", "import os\nC = os.environ[\"NARROW\"]\n"),
            ],
        );
        let surface = env_vars(&repo, &facts);
        assert_eq!(surface[0], ("WIDE".to_string(), 2));
        assert_eq!(surface[1], ("NARROW".to_string(), 1));
        std::fs::remove_dir_all(&repo).ok();
    }
}

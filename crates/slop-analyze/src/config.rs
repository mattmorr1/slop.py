//! The environment surface: which variables the repo reads, and from how many
//! modules. Resource identity names the variable, so the world model can tell
//! an agent where configuration already lives before it adds another read.

use std::collections::BTreeMap;
use std::path::Path;

use slop_parse::{resources, Language};

use crate::source::{is_test_file, FileFacts};

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

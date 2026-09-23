//! `slop calibrate`: fit the relevance model (ADR 0005) on this repository's own
//! co-change history, and validate it on the newest commits before it replaces
//! the model in use.
//!
//! A commit that changed 2–20 functions says each needed the others in view.
//! Historical functions map to today's entities by (file, name, ordinal among
//! same-named functions in that file); an ambiguous name is skipped rather
//! than guessed. Features come from the envelope's own `candidate_features`,
//! so training and serving cannot drift apart.
//!
//! The fit is shrunk toward the model in use (a Gaussian prior on the weights):
//! a small history stays near the pooled model, a large one moves as far as its
//! data supports. The strength is chosen on an inner time split of the training
//! commits, so the reported holdout never informs it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use petgraph::graph::NodeIndex;
use slop_graph::NodeType;
use slop_parse::Language;

use crate::envelope::{self, candidate_features, EnvelopeConfig, Relevance, Selection};
use crate::relevance::{entity_text, RelevanceModel, FEATURES, MODEL_FILE};
use crate::snapshot::RepositorySnapshot;
use crate::source::{is_test_entity, is_test_file};

const NEGATIVES_PER_TASK: usize = 200;
/// Prior strengths tried, from nearly free to nearly the prior itself.
const LAMBDAS: [f64; 5] = [1.0, 10.0, 100.0, 1_000.0, 10_000.0];
const MIN_TASKS: usize = 100;
const MIN_HOLDOUT_TASKS: usize = 20;
const SEED: u64 = 20_260_923;

pub struct CalibrateRequest {
    pub max_commits: usize,
    /// Share of the newest commits held out for validation.
    pub holdout: f64,
    pub budget: usize,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub commit: String,
    pub time: i64,
    pub target: NodeIndex,
    pub gold: Vec<NodeIndex>,
}

/// Mean recall with a commit-cluster bootstrap 95% interval.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Estimate {
    pub mean: f64,
    pub low: f64,
    pub high: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub tasks: usize,
    pub commits: usize,
    pub train_tasks: usize,
    pub holdout_tasks: usize,
    pub budget: usize,
    /// Holdout recall of the fit on older commits, the model in use, and file proximity.
    pub own: Estimate,
    pub current: Estimate,
    pub proximity: Estimate,
    /// Paired per-task difference, own minus current.
    pub own_minus_current: Estimate,
    /// Prior strength chosen on the inner split.
    pub lambda: f64,
    pub current_source: String,
    /// Fit on every task, to be written if the holdout supports it.
    pub model: RelevanceModel,
}

impl Report {
    pub fn supports_writing(&self) -> bool {
        self.own.mean >= self.current.mean
    }
}

pub fn calibrate(snapshot: &RepositorySnapshot, repo: &Path, request: &CalibrateRequest) -> Result<Report> {
    let mut tasks = mine(repo, snapshot, request.max_commits)?;
    if tasks.len() < MIN_TASKS {
        bail!("only {} co-change tasks in the last {} commits; at least {MIN_TASKS} are needed to fit", tasks.len(), request.max_commits);
    }
    tasks.sort_by(|a, b| (a.time, &a.commit).cmp(&(b.time, &b.commit)));
    let mut commits: Vec<&str> = Vec::new();
    for task in &tasks {
        if commits.last() != Some(&task.commit.as_str()) {
            commits.push(&task.commit);
        }
    }
    let all: Vec<&Task> = tasks.iter().collect();
    let (train, holdout) = split(&all, &commits, request.holdout);
    if holdout.len() < MIN_HOLDOUT_TASKS {
        bail!("only {} holdout tasks; widen --holdout or --max-commits", holdout.len());
    }
    let prior: [f64; 12] = snapshot.relevance.weights.as_slice().try_into().context("model in use must have 12 weights")?;
    let rows = |tasks: &[&Task]| training_rows(snapshot, tasks);
    let fitted = |tasks: &[&Task], lambda| fit(&rows(tasks), &prior, lambda).map(|w| model(w, String::new()));
    let mean_recall = |tasks: &[&Task], m: &RelevanceModel| {
        tasks.iter().map(|task| recall(snapshot, task, &envelope_picks(snapshot, task, m, request.budget))).sum::<f64>() / tasks.len().max(1) as f64
    };

    // Inner split of the training commits picks the prior strength; ties go to the stronger prior.
    let train_commits: Vec<&str> = commits.iter().copied().filter(|c| train.iter().any(|t| t.commit == *c)).collect();
    let (inner_train, inner_val) = split(&train, &train_commits, 0.25);
    let mut lambda = LAMBDAS[LAMBDAS.len() - 1];
    let mut best = f64::NEG_INFINITY;
    for &candidate in LAMBDAS.iter().rev() {
        let value = mean_recall(&inner_val, &fitted(&inner_train, candidate)?);
        if value > best + 1e-9 {
            (best, lambda) = (value, candidate);
        }
    }

    let own = fitted(&train, lambda)?;
    let catalog: HashMap<String, envelope::CatalogEntry> =
        envelope::catalog(&snapshot.built, &snapshot.facts).into_iter().map(|entry| (entry.entity.clone(), entry)).collect();
    let score = |select: &dyn Fn(&Task) -> HashSet<String>| -> Vec<(String, f64)> {
        holdout.iter().map(|task| (task.commit.clone(), recall(snapshot, task, &select(task)))).collect()
    };
    let own_scores = score(&|task| envelope_picks(snapshot, task, &own, request.budget));
    let current_scores = score(&|task| envelope_picks(snapshot, task, &snapshot.relevance, request.budget));
    let proximity_scores = score(&|task| proximity_picks(snapshot, &catalog, task, request.budget));
    let paired: Vec<(String, f64)> =
        own_scores.iter().zip(&current_scores).map(|((commit, a), (_, b))| (commit.clone(), a - b)).collect();

    let head = git(repo, &["rev-parse", "--short", "HEAD"])?;
    let name = repo.canonicalize().ok().and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
    let (own_est, current_est) = (bootstrap(&own_scores), bootstrap(&current_scores));
    let source = format!(
        "slop calibrate: {name}@{}, {} tasks from {} commits, prior strength {lambda}; holdout recall@{} {:.1}% vs {:.1}% for the model it replaced",
        head.trim(), tasks.len(), commits.len(), request.budget, own_est.mean * 100.0, current_est.mean * 100.0
    );
    Ok(Report {
        tasks: tasks.len(),
        commits: commits.len(),
        train_tasks: train.len(),
        holdout_tasks: holdout.len(),
        budget: request.budget,
        own: own_est,
        current: current_est,
        proximity: bootstrap(&proximity_scores),
        own_minus_current: bootstrap(&paired),
        lambda,
        current_source: snapshot.relevance.source.clone(),
        model: RelevanceModel { source, ..fitted(&all, lambda)? },
    })
}

/// Older and newest tasks, cutting at the newest `share` of `commits` (oldest first).
fn split<'a>(tasks: &[&'a Task], commits: &[&str], share: f64) -> (Vec<&'a Task>, Vec<&'a Task>) {
    let newest = ((commits.len() as f64 * share).ceil() as usize).clamp(1, commits.len().max(1));
    let cut: HashSet<&str> = commits[commits.len().saturating_sub(newest)..].iter().copied().collect();
    let (newer, older): (Vec<&Task>, Vec<&Task>) = tasks.iter().partition(|task| cut.contains(task.commit.as_str()));
    (older, newer)
}

/// Write the fitted model where the snapshot looks for it.
pub fn write(repo: &Path, model: &RelevanceModel) -> Result<PathBuf> {
    let path = repo.join(MODEL_FILE);
    std::fs::create_dir_all(path.parent().expect("model file has a parent"))?;
    std::fs::write(&path, serde_json::to_string_pretty(model)? + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn model(weights: [f64; 12], source: String) -> RelevanceModel {
    RelevanceModel {
        features: FEATURES.iter().map(|name| name.to_string()).collect(),
        weights: weights.iter().map(|w| (w * 1e6).round() / 1e6).collect(),
        source,
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One commit's added-line ranges (1-based, inclusive) per file, from `git log -p --unified=0`.
#[derive(Debug, Default, PartialEq)]
struct Change {
    sha: String,
    time: i64,
    ranges: BTreeMap<String, Vec<(u32, u32)>>,
}

const MARK: &str = "\u{1f}C ";

fn parse_log(text: &str) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    let mut file: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(MARK) {
            let mut parts = rest.split_whitespace();
            let (Some(sha), Some(time)) = (parts.next(), parts.next().and_then(|t| t.parse().ok())) else { continue };
            changes.push(Change { sha: sha.to_string(), time, ranges: BTreeMap::new() });
            file = None;
        } else if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_string);
        } else if let (Some(hunk), Some(path), Some(change)) = (line.strip_prefix("@@ "), &file, changes.last_mut()) {
            let Some(added) = hunk.split_whitespace().find_map(|part| part.strip_prefix('+')) else { continue };
            let mut numbers = added.split(',');
            let start: u32 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let count: u32 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(1);
            change.ranges.entry(path.clone()).or_default().push((start, start + count.max(1) - 1));
        }
    }
    changes
}

/// File contents at (sha, path), through one `git cat-file --batch`; None where absent.
fn contents(repo: &Path, requests: &[(String, String)]) -> Result<Vec<Option<String>>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running git cat-file")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input: String = requests.iter().map(|(sha, path)| format!("{sha}:{path}\n")).collect();
    // Written from a thread: a full stdout pipe would otherwise deadlock the writer.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut out = Vec::with_capacity(requests.len());
    for _ in requests {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        match fields.as_slice() {
            [_, "blob", size] => {
                let mut body = vec![0; size.parse::<usize>()? + 1];
                reader.read_exact(&mut body)?;
                body.pop();
                out.push(Some(String::from_utf8_lossy(&body).into_owned()));
            }
            _ => out.push(None),
        }
    }
    writer.join().expect("cat-file writer panicked")?;
    if !child.wait()?.success() {
        bail!("git cat-file failed");
    }
    Ok(out)
}

pub fn mine(repo: &Path, snapshot: &RepositorySnapshot, max_commits: usize) -> Result<Vec<Task>> {
    let graph = &snapshot.built.graph;
    let mut current: HashMap<&str, HashMap<&str, Vec<NodeIndex>>> = HashMap::new();
    for (idx, entity) in graph.entities() {
        if entity.entity_type == NodeType::Function && !is_test_file(&entity.file) && !is_test_entity(&entity.id) {
            current.entry(entity.file.as_str()).or_default().entry(entity.name.as_str()).or_default().push(idx);
        }
    }
    for names in current.values_mut() {
        for list in names.values_mut() {
            list.sort_by_key(|&idx| graph.entity(idx).source_range.0);
        }
    }
    let log = git(repo, &["log", "--no-merges", &format!("-n{max_commits}"), &format!("--format={MARK}%H %ct"), "-p", "--unified=0", "--no-color", "--no-renames"])?;
    let changes = parse_log(&log);
    let requests: Vec<(String, String)> = changes
        .iter()
        .flat_map(|change| {
            change.ranges.keys().filter(|path| current.contains_key(path.as_str())).map(|path| (change.sha.clone(), path.clone()))
        })
        .collect();
    let mut files = requests.iter().zip(contents(repo, &requests)?);
    let mut tasks = Vec::new();
    for change in &changes {
        let mut changed: Vec<NodeIndex> = Vec::new();
        for (path, ranges) in &change.ranges {
            let Some(names) = current.get(path.as_str()) else { continue };
            let Some((_, text)) = files.next() else { break };
            let Some(functions) = text.as_deref().and_then(|text| Language::from_path(path)?.parse(text).ok()) else { continue };
            let mut ordinal: HashMap<&str, usize> = HashMap::new();
            let mut count: HashMap<&str, usize> = HashMap::new();
            functions.iter().for_each(|f| *count.entry(f.name.as_str()).or_default() += 1);
            let mut ordered: Vec<_> = functions.iter().collect();
            ordered.sort_by_key(|f| f.start_line);
            for function in ordered {
                let k = ordinal.entry(function.name.as_str()).or_default();
                let (first, last) = (function.start_line + 1, function.end_line + 1);
                let touched = ranges.iter().any(|&(lo, hi)| lo <= last && first <= hi);
                if let (true, Some(list)) = (touched, names.get(function.name.as_str())) {
                    if list.len() == count[function.name.as_str()] {
                        changed.push(list[*k]);
                    }
                }
                *k += 1;
            }
        }
        changed.sort();
        changed.dedup();
        if (2..=20).contains(&changed.len()) {
            for &target in &changed {
                let gold = changed.iter().copied().filter(|&idx| idx != target).collect();
                tasks.push(Task { commit: change.sha.clone(), time: change.time, target, gold });
            }
        }
    }
    Ok(tasks)
}

/// Deterministic splitmix64: reproducible sampling without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

type Row = ([f64; 12], f64, f64);

/// Positives, plus up to 200 sampled negatives weighted back up so probabilities stay calibrated.
fn training_rows(snapshot: &RepositorySnapshot, tasks: &[&Task]) -> Vec<Row> {
    let mut rng = Rng(SEED);
    let mut rows = Vec::new();
    for task in tasks {
        let entity = snapshot.built.graph.entity(task.target);
        let text = snapshot.sources.get(&entity.file).and_then(|source| entity_text(source, entity)).unwrap_or_default();
        let gold: HashSet<NodeIndex> = task.gold.iter().copied().collect();
        let (positives, mut negatives): (Vec<_>, Vec<_>) = candidate_features(&snapshot.built, task.target, snapshot.lexical(), &text)
            .into_iter()
            .partition(|(idx, _)| gold.contains(idx));
        let total = negatives.len();
        for i in 0..NEGATIVES_PER_TASK.min(total) {
            let j = i + rng.below(total - i);
            negatives.swap(i, j);
        }
        negatives.truncate(NEGATIVES_PER_TASK);
        let scale = total as f64 / negatives.len().max(1) as f64;
        rows.extend(positives.iter().map(|(_, f)| (f.values(), 1.0, 1.0)));
        rows.extend(negatives.iter().map(|(_, f)| (f.values(), 0.0, scale)));
    }
    rows
}

/// Logistic regression by Newton's method (IRLS), MAP under a Gaussian prior centred
/// on `prior` with precision `lambda`; the bias is unpenalised.
fn fit(rows: &[Row], prior: &[f64; 12], lambda: f64) -> Result<[f64; 12]> {
    let mut beta = *prior;
    for _ in 0..30 {
        let (mut gradient, mut hessian) = ([0.0; 12], [[0.0; 12]; 12]);
        for (x, y, w) in rows {
            let p = 1.0 / (1.0 + (-x.iter().zip(&beta).map(|(a, b)| a * b).sum::<f64>()).exp());
            for i in 0..12 {
                gradient[i] += w * (p - y) * x[i];
                for j in 0..12 {
                    hessian[i][j] += w * p * (1.0 - p) * x[i] * x[j];
                }
            }
        }
        for i in 1..12 {
            gradient[i] += lambda * (beta[i] - prior[i]);
            hessian[i][i] += lambda;
        }
        let step = solve(hessian, gradient).context("singular Hessian: the history gives no signal for the bias")?;
        beta.iter_mut().zip(&step).for_each(|(b, s)| *b -= s);
        if step.iter().all(|s| s.abs() < 1e-8) {
            break;
        }
    }
    Ok(beta)
}

/// Gaussian elimination with partial pivoting; None if singular.
fn solve(mut a: [[f64; 12]; 12], mut b: [f64; 12]) -> Option<[f64; 12]> {
    for col in 0..12 {
        let pivot = (col..12).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..12 {
            let factor = a[row][col] / a[col][col];
            let pivot_row = a[col];
            a[row][col..].iter_mut().zip(&pivot_row[col..]).for_each(|(value, p)| *value -= factor * p);
            b[row] -= factor * b[col];
        }
    }
    let mut x = [0.0; 12];
    for row in (0..12).rev() {
        x[row] = (b[row] - (row + 1..12).map(|k| a[row][k] * x[k]).sum::<f64>()) / a[row][row];
    }
    Some(x)
}

/// What the envelope shows under `model`, filling the budget so ranking alone is compared.
fn envelope_picks(snapshot: &RepositorySnapshot, task: &Task, model: &RelevanceModel, budget: usize) -> HashSet<String> {
    let target = &snapshot.built.graph.entity(task.target).id;
    let config = EnvelopeConfig { token_budget: budget, edit_zone_hops: 1, selection: Selection::Coverage, min_probability_ppm: 0 };
    let relevance = Relevance { model, lexical: snapshot.lexical() };
    let (items, _) =
        envelope::build_captured_envelope(&snapshot.built, &snapshot.facts, &snapshot.sources, target, &config, &relevance, |_| true);
    items.into_iter().map(|item| item.entity).filter(|entity| entity != target).collect()
}

/// Same file by line distance, then the rest of the directory, packed at skeleton cost.
fn proximity_picks(snapshot: &RepositorySnapshot, catalog: &HashMap<String, envelope::CatalogEntry>, task: &Task, budget: usize) -> HashSet<String> {
    let target = snapshot.built.graph.entity(task.target);
    let dir = |file: &str| file.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    let target_dir = dir(&target.file);
    let mut near: Vec<&envelope::CatalogEntry> =
        catalog.values().filter(|entry| entry.entity != target.id && dir(&entry.file) == target_dir).collect();
    near.sort_by_key(|entry| (entry.file != target.file, entry.lines.0.abs_diff(target.source_range.0), entry.entity.clone()));
    let mut spent = 0;
    near.into_iter()
        .filter(|entry| {
            let fits = spent + entry.skeleton_tokens <= budget;
            spent += if fits { entry.skeleton_tokens } else { 0 };
            fits
        })
        .map(|entry| entry.entity.clone())
        .collect()
}

fn recall(snapshot: &RepositorySnapshot, task: &Task, picked: &HashSet<String>) -> f64 {
    let hits = task.gold.iter().filter(|&&idx| picked.contains(&snapshot.built.graph.entity(idx).id)).count();
    hits as f64 / task.gold.len() as f64
}

/// Mean over tasks; interval from resampling whole commits, since one commit's tasks are correlated.
fn bootstrap(scores: &[(String, f64)]) -> Estimate {
    let mut clusters: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    scores.iter().for_each(|(commit, value)| clusters.entry(commit).or_default().push(*value));
    let clusters: Vec<Vec<f64>> = clusters.into_values().collect();
    let mean = scores.iter().map(|(_, v)| v).sum::<f64>() / scores.len().max(1) as f64;
    let mut rng = Rng(SEED);
    let mut means: Vec<f64> = (0..1000)
        .map(|_| {
            let (mut sum, mut n) = (0.0, 0usize);
            for _ in 0..clusters.len() {
                let cluster = &clusters[rng.below(clusters.len())];
                sum += cluster.iter().sum::<f64>();
                n += cluster.len();
            }
            sum / n.max(1) as f64
        })
        .collect();
    means.sort_by(f64::total_cmp);
    Estimate { mean, low: means[25], high: means[974] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_parsing_keeps_added_ranges_per_file() {
        let log = format!(
            "{MARK}abc 100\ndiff --git a/x.py b/x.py\n--- a/x.py\n+++ b/x.py\n@@ -3,2 +3,4 @@ def f():\n+a\n@@ -20 +22 @@\n-b\n\
             +++ /dev/null\n@@ -1,3 +0,0 @@\n{MARK}def 90\n+++ b/y.py\n@@ -5,0 +6,0 @@\n"
        );
        let changes = parse_log(&log);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].ranges["x.py"], vec![(3, 6), (22, 22)]);
        assert_eq!(changes[0].ranges.len(), 1, "a deleted file contributes nothing");
        assert_eq!((changes[1].sha.as_str(), changes[1].time), ("def", 90));
        assert_eq!(changes[1].ranges["y.py"], vec![(6, 6)], "a pure deletion marks its position");
    }

    #[test]
    fn irls_recovers_a_known_model() {
        let truth = [-2.0, 1.5, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut rng = Rng(7);
        let rows: Vec<Row> = (0..20_000)
            .map(|_| {
                let mut x = [0.0; 12];
                x[0] = 1.0;
                x[1] = (rng.below(2)) as f64;
                x[5] = (rng.below(2)) as f64;
                let p = 1.0 / (1.0 + (-x.iter().zip(&truth).map(|(a, b)| a * b).sum::<f64>()).exp());
                let y = f64::from(u8::from((rng.next() as f64 / u64::MAX as f64) < p));
                (x, y, 1.0)
            })
            .collect();
        let beta = fit(&rows, &[0.0; 12], 1.0).expect("fit");
        for i in [0, 1, 5] {
            assert!((beta[i] - truth[i]).abs() < 0.1, "feature {i}: {} vs {}", beta[i], truth[i]);
        }
        assert!(beta[2].abs() < 1e-6, "a feature that never fires stays at its prior");
        let prior = [0.0, -1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let shrunk = fit(&rows, &prior, 1e7).expect("fit");
        assert!((shrunk[1] - prior[1]).abs() < 0.05 && (shrunk[5] - prior[5]).abs() < 0.05, "a strong prior holds the weights");
    }

    #[test]
    fn bootstrap_interval_brackets_the_mean() {
        let scores: Vec<(String, f64)> = (0..60).map(|i| (format!("c{}", i / 3), f64::from(i % 5) / 4.0)).collect();
        let estimate = bootstrap(&scores);
        assert!(estimate.low <= estimate.mean && estimate.mean <= estimate.high);
        assert!(estimate.high - estimate.low > 0.0);
    }
}

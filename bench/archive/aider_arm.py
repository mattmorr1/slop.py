"""B4 competitor arm: Aider's repo map (aider-chat pinned below) on B4's own tasks.

Runs in an environment with aider-chat installed, apart from the main bench:

    uv venv --python 3.12 aider-venv && VIRTUAL_ENV=aider-venv uv pip install aider-chat==0.86.1
    aider-venv/bin/python bench/aider_arm.py --repo NAME=SNAPSHOT --targets DIR/NAME.txt --out DIR/NAME.jsonl

Aider's ranking and rendering are used unchanged; only its tokenizer is set to
B4's chars/4 estimate so budgets share units. Two variants per budget:
`aider-map` is the map alone; `aider-chat` is Aider as used, with the target's
file in the chat (shown whole, excluded from the map) charged to the budget.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import aider
from aider.repomap import RepoMap

PINNED = "0.86.1"
SLOP = Path(os.environ.get("SLOP_BIN", Path(__file__).resolve().parents[1] / "target/release/slop"))
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def tokens(text: str) -> int:
    return len(text) // 4 + 1


class Model:
    def token_count(self, text: str) -> int:
        return tokens(text)


class IO:
    def read_text(self, fname):
        try:
            return Path(fname).read_text(errors="replace")
        except OSError as error:
            print(f"aider_arm: cannot read {fname}: {error}", file=sys.stderr)
            return None

    def tool_output(self, *args, **kwargs):
        pass

    def tool_warning(self, message="", **kwargs):
        print(f"aider warning: {message}", file=sys.stderr)

    def tool_error(self, message="", **kwargs):
        print(f"aider error: {message}", file=sys.stderr)


def simple_name(entity: str) -> str:
    return IDENT.findall(entity.rsplit("::", 1)[-1])[-1]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True, help="NAME=SNAPSHOT")
    parser.add_argument("--targets", type=Path, required=True)
    parser.add_argument("--budgets", default="1000,2000,4000,8000,16000")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if aider.__version__ != PINNED:
        raise SystemExit(f"aider-chat {aider.__version__} installed; this arm is pinned to {PINNED}")
    _, snapshot = args.repo.split("=", 1)
    root = Path(snapshot).resolve()
    budgets = [int(b) for b in args.budgets.split(",")]

    header = subprocess.run([str(SLOP), "context-bench", str(root), "/dev/null", "--budgets", "1000"],
                            capture_output=True, text=True, check=True).stdout.splitlines()[0]
    catalog = {entry["entity"]: entry for entry in json.loads(header)["catalog"]}
    by_file: dict[str, list[dict]] = {}
    for entry in catalog.values():
        by_file.setdefault(entry["file"], []).append(entry)
    files = sorted(by_file)

    def entity_at(rel: str, line: int, name: str) -> str | None:
        """The innermost catalog entity in `rel` named `name` whose range holds `line`."""
        hits = [e for e in by_file.get(rel, ()) if e["lines"][0] <= line <= e["lines"][1]
                and simple_name(e["entity"]) == name]
        return max(hits, key=lambda e: e["lines"][0])["entity"] if hits else None

    repo_map = RepoMap(map_tokens=1024, root=str(root), main_model=Model(), io=IO(), refresh="always")
    ranked_cache: dict = {}
    original = repo_map.get_ranked_tags

    def ranked(chat, other, mentioned_fnames, mentioned_idents, progress=None):
        key = (tuple(chat), tuple(sorted(mentioned_idents)))
        if key not in ranked_cache:
            ranked_cache.clear()
            ranked_cache[key] = original(chat, other, mentioned_fnames, mentioned_idents, progress)
        return ranked_cache[key]

    repo_map.get_ranked_tags = ranked
    trees: dict[str, list] = {}
    render = repo_map.to_tree

    def to_tree(tags, chat_rel_fnames):
        tree = render(tags, chat_rel_fnames)
        trees[tree] = tags
        return tree

    repo_map.to_tree = to_tree

    def repo_map_for(target_file: str, name: str, budget: int) -> tuple[set[str], int]:
        if budget <= 0:
            return set(), 0
        chat = [str(root / target_file)]
        other = [str(root / f) for f in files if f != target_file]
        trees.clear()
        tree = repo_map.get_ranked_tags_map_uncached(chat, other, budget, set(), {name}) or ""
        picked = {entity_at(tag.rel_fname, tag.line, tag.name) for tag in trees.get(tree, [])
                  if hasattr(tag, "kind") and tag.kind == "def"}
        return picked - {None}, tokens(tree) if tree else 0

    targets = [t for t in args.targets.read_text().split() if t in catalog]
    with args.out.open("w") as out:
        for n, target in enumerate(targets):
            entry = catalog[target]
            name = simple_name(target)
            source = (root / entry["file"]).read_text(errors="replace")
            same_file = {e["entity"] for e in by_file[entry["file"]]} - {target}
            for budget in budgets:
                picked, spent = repo_map_for(entry["file"], name, budget)
                out.write(json.dumps({"target": target, "budget": budget, "arm": "aider-map",
                                      "items": sorted(picked - {target}), "tokens": spent}) + "\n")
                file_cost = tokens(source)
                picked, spent = repo_map_for(entry["file"], name, budget - file_cost)
                out.write(json.dumps({"target": target, "budget": budget, "arm": "aider-chat",
                                      "items": sorted((picked | same_file) - {target}),
                                      "tokens": spent + file_cost}) + "\n")
            print(f"{n + 1}/{len(targets)} {target}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()

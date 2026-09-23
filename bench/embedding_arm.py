"""B4 competitor arm: dense retrieval (bge-small-en-v1.5 via fastembed) on B4's tasks.

The "why not embeddings?" baseline. Every function and class is embedded once
from its source; a target's candidates are ranked by cosine similarity and
packed by the same skeleton cost the other Python arms pay.

    uv venv --python 3.12 embed-venv && VIRTUAL_ENV=embed-venv uv pip install fastembed==0.7.3
    embed-venv/bin/python bench/embedding_arm.py --repo NAME=SNAPSHOT --targets DIR/NAME.txt --out DIR/NAME.jsonl
"""

import argparse
import json
import os
import subprocess
from pathlib import Path

import numpy as np
from fastembed import TextEmbedding

MODEL = "BAAI/bge-small-en-v1.5"
SLOP = Path(os.environ.get("SLOP_BIN", Path(__file__).resolve().parents[1] / "target/release/slop"))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True, help="NAME=SNAPSHOT")
    parser.add_argument("--targets", type=Path, required=True)
    parser.add_argument("--budgets", default="1000,2000,4000,8000,16000")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    _, snapshot = args.repo.split("=", 1)
    root = Path(snapshot).resolve()
    budgets = [int(b) for b in args.budgets.split(",")]

    header = subprocess.run([str(SLOP), "context-bench", str(root), "/dev/null", "--budgets", "1000"],
                            capture_output=True, text=True, check=True).stdout.splitlines()[0]
    catalog = sorted(json.loads(header)["catalog"], key=lambda entry: entry["entity"])
    files: dict[str, list[str]] = {}
    texts = []
    for entry in catalog:
        if entry["file"] not in files:
            files[entry["file"]] = (root / entry["file"]).read_text(errors="replace").splitlines()
        first, last = entry["lines"]
        texts.append("\n".join(files[entry["file"]][first: last + 1]) or entry["entity"])
    vectors = np.array(list(TextEmbedding(MODEL).embed(texts, batch_size=64)), dtype=np.float32)
    vectors /= np.linalg.norm(vectors, axis=1, keepdims=True).clip(min=1e-12)
    index = {entry["entity"]: i for i, entry in enumerate(catalog)}
    cost = np.array([entry["skeleton_tokens"] for entry in catalog])

    targets = [t for t in args.targets.read_text().split() if t in index]
    with args.out.open("w") as out:
        for target in targets:
            t = index[target]
            similarity = vectors @ vectors[t]
            similarity[t] = -np.inf
            # Stable sort over entity-sorted rows: ties break by entity id.
            order = np.argsort(-similarity, kind="stable")
            for budget in budgets:
                picked, spent = [], 0
                for i in order:
                    if spent + cost[i] <= budget:
                        picked.append(catalog[i]["entity"])
                        spent += int(cost[i])
                out.write(json.dumps({"target": target, "budget": budget, "arm": "embedding-bge-small",
                                      "items": sorted(picked), "tokens": spent}) + "\n")


if __name__ == "__main__":
    main()

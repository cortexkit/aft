#!/usr/bin/env python3
"""Report the parts of a GitHub Actions workflow that decide whether a train's
CI run means anything.

train-push.sh calls this before pushing a train. Two questions are answered:

  1. does `on.push.branches` match the train branch about to be pushed, and
  2. which job-level and step-level `if:` conditions gate on something the
     landing path never satisfies.

Both answers have to come from the parsed document rather than from matching
lines. `if:` is very often written as a folded scalar:

    if: >-
      github.ref == 'refs/heads/main'

so a line-oriented search for `if:.*github.ref` finds nothing on exactly the
workflows whose behaviour depends on the ref - the ones this check exists for.
The same goes for a `branches:` list written in flow style.

PyYAML is not a dependency of this repository and a gate that behaves
differently depending on whether it happens to be installed is worse than no
gate, so the block/flow subset that workflow files actually use is parsed here.

Output is one record per line for the shell to consume:

    trigger|ok                      the push trigger matches the train ref
    trigger|missing                 it does not (no push trigger, or no match)
    ref|<text>                      a finding that refuses the push
    warn|<text>                     a finding that is reported and no more

Usage:
  workflow-gates.py --train-ref train/foo --tests-workflow FILE WORKFLOW...
"""

from __future__ import annotations

import fnmatch
import sys
from typing import Any

BLOCK_SCALAR_HEADS = ("|", ">", "|-", ">-", "|+", ">+")


# --------------------------------------------------------------------------
# A parser for the YAML subset workflow files are written in: block mappings,
# block sequences, flow sequences, quoted and plain scalars, and block scalars.
# --------------------------------------------------------------------------
def _strip_comment(text: str) -> str:
    """Drop a trailing comment, ignoring '#' inside quotes or ${{ }}."""
    quote = ""
    for i, ch in enumerate(text):
        if quote:
            if ch == quote:
                quote = ""
            continue
        if ch in "'\"":
            quote = ch
            continue
        if ch == "#" and (i == 0 or text[i - 1] in " \t"):
            return text[:i]
    return text


def _split_key(text: str) -> tuple[str, str] | None:
    """Split 'key: value' at the first colon that is not inside quotes."""
    quote = ""
    for i, ch in enumerate(text):
        if quote:
            if ch == quote:
                quote = ""
            continue
        if ch in "'\"":
            quote = ch
            continue
        if ch == ":" and (i + 1 == len(text) or text[i + 1] in " \t"):
            return text[:i].strip(), text[i + 1 :].strip()
    return None


def _scalar(text: str) -> str:
    text = text.strip()
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"":
        return text[1:-1]
    return text


def _flow_sequence(text: str) -> list[str]:
    inner = text.strip()[1:-1]
    return [_scalar(part) for part in inner.split(",") if part.strip()]


class _Reader:
    def __init__(self, source: str) -> None:
        self.lines = source.splitlines()
        self.i = 0

    def indent(self, index: int) -> int:
        line = self.lines[index]
        return len(line) - len(line.lstrip(" "))

    def skip_blanks(self) -> None:
        while self.i < len(self.lines):
            stripped = _strip_comment(self.lines[self.i]).strip()
            if stripped:
                return
            self.i += 1

    def at_end(self) -> bool:
        self.skip_blanks()
        return self.i >= len(self.lines)

    def block_scalar(self, head: str, parent_indent: int) -> str:
        """Collect the lines of a `|`/`>` scalar and fold them like YAML does."""
        collected: list[str] = []
        while self.i < len(self.lines):
            line = self.lines[self.i]
            if not line.strip():
                collected.append("")
                self.i += 1
                continue
            if self.indent(self.i) <= parent_indent:
                break
            collected.append(line.strip())
            self.i += 1
        if head.startswith(">"):
            return " ".join(part for part in collected if part)
        return "\n".join(collected)

    def parse(self, indent: int) -> Any:
        if self.at_end():
            return None
        if _strip_comment(self.lines[self.i]).strip().startswith("- "):
            return self.parse_sequence(indent)
        if _strip_comment(self.lines[self.i]).strip() == "-":
            return self.parse_sequence(indent)
        return self.parse_mapping(indent)

    def parse_sequence(self, indent: int) -> list[Any]:
        items: list[Any] = []
        while not self.at_end():
            if self.indent(self.i) != indent:
                break
            content = _strip_comment(self.lines[self.i]).strip()
            if not content.startswith("-"):
                break
            rest = content[1:].strip()
            item_indent = self.indent(self.i) + (len(content) - len(content[1:].lstrip()))
            if not rest:
                self.i += 1
                items.append(self.parse(indent + 2) if not self.at_end() else None)
                continue
            pair = _split_key(rest)
            if pair is None:
                items.append(_scalar(rest))
                self.i += 1
                continue
            # `- key: value` starts a mapping whose keys line up after the dash.
            self.lines[self.i] = " " * item_indent + rest
            items.append(self.parse_mapping(item_indent))
        return items

    def parse_mapping(self, indent: int) -> dict[str, Any]:
        mapping: dict[str, Any] = {}
        while not self.at_end():
            if self.indent(self.i) != indent:
                break
            content = _strip_comment(self.lines[self.i]).strip()
            if content.startswith("- "):
                break
            pair = _split_key(content)
            if pair is None:
                self.i += 1
                continue
            key, value = pair
            key = _scalar(key)
            self.i += 1
            if value in BLOCK_SCALAR_HEADS:
                mapping[key] = self.block_scalar(value, indent)
            elif value.startswith("[") and value.endswith("]"):
                mapping[key] = _flow_sequence(value)
            elif value:
                mapping[key] = _scalar(value)
            elif self.at_end() or self.indent(self.i) <= indent:
                mapping[key] = None
            else:
                mapping[key] = self.parse(self.indent(self.i))
        return mapping


def parse_workflow(path: str) -> dict[str, Any]:
    with open(path, encoding="utf-8") as handle:
        document = _Reader(handle.read()).parse(0)
    return document if isinstance(document, dict) else {}


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------
def as_list(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item) for item in value]
    return [str(value)]


def on_section(workflow: dict[str, Any]) -> Any:
    # `on` is also YAML's boolean true, which some parsers hand back as a
    # different key; accept both spellings rather than silently reading nothing.
    for key in ("on", True, "true", "True"):
        if key in workflow:
            return workflow[key]
    return None


def trigger_summary(workflow: dict[str, Any]) -> str:
    section = on_section(workflow)
    if section is None:
        return "?"
    if isinstance(section, str):
        return section
    if isinstance(section, list):
        return ", ".join(section)

    parts = []
    for event, body in section.items():
        detail = ""
        if isinstance(body, dict):
            branches = as_list(body.get("branches"))
            tags = as_list(body.get("tags"))
            if branches:
                detail = "branches: " + ", ".join(branches)
            if tags:
                detail = (detail + "; " if detail else "") + "tags: " + ", ".join(tags)
        if not detail and event in ("push", "pull_request"):
            detail = "branches: all"
        parts.append(f"{event} ({detail})" if detail else event)
    return ", ".join(parts) or "?"


def push_matches(workflow: dict[str, Any], train_ref: str) -> bool:
    section = on_section(workflow)
    if isinstance(section, str):
        return section.strip() == "push"
    if isinstance(section, list):
        return "push" in section
    if not isinstance(section, dict) or "push" not in section:
        return False
    body = section["push"]
    if not isinstance(body, dict):
        # `push:` with nothing under it triggers on every branch.
        return True
    branches = as_list(body.get("branches"))
    if not branches:
        # A push trigger with no branches list runs on every branch. A
        # branches-ignore list is a filter we do not interpret; treat it as a
        # match and let the trigger probe be the judge.
        return True
    return any(fnmatch.fnmatchcase(train_ref, pattern) for pattern in branches)


def step_label(step: Any) -> str:
    if not isinstance(step, dict):
        return ""
    for key in ("name", "id", "uses", "run"):
        value = step.get(key)
        if isinstance(value, str) and value.strip():
            label = value.strip().splitlines()[0]
            return label if len(label) <= 48 else label[:45] + "..."
    return ""


def classify(condition: str) -> str | None:
    """ref = provably dead under trains, warn = depends on the landing path."""
    if "github.ref" in condition:
        if "train/" in condition:
            return None
        tag_only = "refs/tags/" in condition and "refs/heads/" not in condition
        if tag_only and "ref_name" not in condition:
            # A tag gate belongs to the release event, not to the landing path,
            # and widening it to train branches would be wrong.
            return "warn"
        return "ref"
    if "github.event_name" in condition or "event_name ==" in condition:
        return "warn"
    return None


def scan(path: str) -> tuple[list[str], list[str], str]:
    workflow = parse_workflow(path)
    refs: list[str] = []
    warns: list[str] = []

    concurrency = workflow.get("concurrency")
    group = ""
    if isinstance(concurrency, dict):
        group = str(concurrency.get("group") or "")
    elif isinstance(concurrency, str):
        group = concurrency
    if group and "github.ref" not in group:
        # A group shared across refs cancels in-flight runs of other refs, so
        # pushing a train would kill a running default-branch run.
        warns.append(f"  concurrency group is not per-ref: {group}")

    jobs = workflow.get("jobs")
    if isinstance(jobs, dict):
        for job_id, job in jobs.items():
            if not isinstance(job, dict):
                continue
            entries = [("", job.get("if"))]
            steps = job.get("steps")
            if isinstance(steps, list):
                entries += [(step_label(s), s.get("if")) for s in steps if isinstance(s, dict)]
            for label, condition in entries:
                if not isinstance(condition, str) or not condition.strip():
                    continue
                condition = " ".join(condition.split())
                kind = classify(condition)
                if kind is None:
                    continue
                where = f"  job {job_id}"
                if label:
                    where += f", step {label}"
                (refs if kind == "ref" else warns).append(f"{where}: {condition}")

    header = f"{path} [on: {trigger_summary(workflow)}]"
    header += f" [concurrency: {group}]" if group else " [concurrency: none]"
    return refs, warns, header


def main(argv: list[str]) -> int:
    train_ref = ""
    tests_workflow = ""
    workflows: list[str] = []
    i = 0
    while i < len(argv):
        if argv[i] == "--train-ref":
            train_ref = argv[i + 1]
            i += 2
        elif argv[i] == "--tests-workflow":
            tests_workflow = argv[i + 1]
            i += 2
        else:
            workflows.append(argv[i])
            i += 1

    if tests_workflow:
        try:
            matched = push_matches(parse_workflow(tests_workflow), train_ref)
        except OSError:
            matched = False
        print("trigger|ok" if matched else "trigger|missing")

    for path in workflows:
        try:
            refs, warns, header = scan(path)
        except OSError:
            continue
        for kind, findings in (("ref", refs), ("warn", warns)):
            if not findings:
                continue
            print(f"{kind}|{header}")
            for finding in findings:
                print(f"{kind}|{finding}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

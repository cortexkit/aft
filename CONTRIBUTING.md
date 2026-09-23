# Contributing

## Design first

Open an issue describing the change you want to make and why. A maintainer
reads it and, once the approach is agreed, adds the `design-approved` label to
the issue. Then open the pull request with `Approved issue: #N` in the
description (the pull request template has the line). Please don't use a
closing keyword such as `Closes #N`: GitHub would close the issue as soon as
the pull request merges, before the release carrying the change ships.
Maintainers close issues when the fix ships.

The `design-gate` check enforces this: a PR without a linked, `design-approved`
issue cannot be reviewed or merged, and a PR opened or marked ready for review
before the label lands is put back into draft. When the label is applied, the
pull request is marked ready for review automatically if the gate is what put
it into draft; a pull request you opened as a draft yourself stays a draft. A
maintainer can label a pull request `trivial` instead when there is no design
to agree. The gate is the fleet's
shared one, run from `cortexkit/subconscious`; the same rules apply on every
CortexKit repository.

Draft PRs are welcome at any time as proposals — open one whenever showing code
is the clearest way to make the point. It stays a draft until its issue is
approved.

Typo-class fixes do not need an issue. Open the PR; a maintainer applies the
`trivial` label to it and the gate stands down.

## Before you push

Run `bun run format` and `cargo fmt`. CI rejects unformatted code.

#!/usr/bin/env node
/**
 * Design-approved gate for contributor pull requests.
 *
 * The rule: a pull request is reviewable and mergeable only once the issue it
 * closes carries a maintainer's `design-approved` label. GitHub has no native
 * "issue first" setting, so the rule is assembled from two primitives — this
 * script's exit code (surfaced as a required status check) and the pull
 * request's draft state.
 *
 * Parsing and decision logic live in this script rather than inline in the
 * workflow so they can be unit tested with no network (design-gate.test.mjs).
 * The workflow only supplies the event payload and a token.
 *
 * Usage:
 *   node scripts/design-gate.mjs pull-request   # decide, comment, exit 0/1
 *   node scripts/design-gate.mjs issue-labeled  # release drafts waiting on an issue
 */

import { appendFileSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const DESIGN_APPROVED_LABEL = "design-approved";
export const TRIVIAL_LABEL = "trivial";

/** Hidden marker that lets a later run find the comment it already posted. */
export const COMMENT_MARKER = "<!-- design-gate -->";

/** The name of the check run published from the issue arm. Must match the job id. */
export const CHECK_NAME = "design-gate";

/**
 * Branches maintainers push directly. These carry no contributor design
 * conversation, so the gate does not apply to them — but only when the head
 * is this repository, so a fork cannot bypass the gate by naming its branch
 * `train/whatever`.
 */
export const MAINTAINER_BRANCH_PREFIXES = ["train", "alfonso"];

/**
 * The only text a blocked contributor reads. Keep it exact: it has to explain
 * the rule, the fix, and the escape hatch without any other context.
 */
export const GATE_MESSAGE = [
  "This PR needs a linked issue with the `design-approved` label before it can be reviewed or",
  "merged. Add `Closes #<issue>` to the description; a maintainer will apply the label on the",
  "issue once the design is agreed. Trivial fixes: a maintainer can add the `trivial` label to",
  "this PR instead.",
].join(" ");

/**
 * GitHub's own closing-keyword grammar: a keyword, optional colon, whitespace,
 * then either `#N`, `owner/repo#N`, or a full issue URL. Matching what GitHub
 * matches means the gate and the "linked issues" sidebar never disagree.
 */
const CLOSING_LINK_PATTERN = new RegExp(
  [
    String.raw`\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b\s*:?\s+`,
    "(?:",
    String.raw`https?://github\.com/(?<urlOwner>[\w.-]+)/(?<urlRepo>[\w.-]+)/issues/(?<urlNumber>\d+)`,
    String.raw`|(?:(?<refOwner>[\w.-]+)/(?<refRepo>[\w.-]+))?#(?<refNumber>\d+)`,
    ")",
  ].join(""),
  "gi",
);

/**
 * GitHub does not turn references inside HTML comments, fenced blocks, or code
 * spans into links, so neither does the gate. Pull request templates routinely
 * carry an example `Closes #` inside an HTML comment; that must not count as a
 * real link.
 */
function stripUnlinkedRegions(body) {
  return body
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`[^`\n]*`/g, " ");
}

function sameRepo(a, b) {
  return Boolean(a) && Boolean(b) && a.toLowerCase() === b.toLowerCase();
}

/**
 * First issue this body closes in `repoFullName`, or null.
 *
 * References to other repositories are not links here at all — they are
 * skipped, and a later same-repo reference still wins.
 *
 * @returns {{ number: number, text: string } | null}
 */
export function parseLinkedIssue(body, repoFullName) {
  if (!body) return null;
  const haystack = stripUnlinkedRegions(body);
  CLOSING_LINK_PATTERN.lastIndex = 0;
  for (const match of haystack.matchAll(CLOSING_LINK_PATTERN)) {
    const groups = match.groups ?? {};
    const owner = groups.urlOwner ?? groups.refOwner;
    const repo = groups.urlRepo ?? groups.refRepo;
    const number = groups.urlNumber ?? groups.refNumber;
    if (owner && !sameRepo(`${owner}/${repo}`, repoFullName)) continue;
    return { number: Number.parseInt(number, 10), text: match[0].trim() };
  }
  return null;
}

/** `train/**` semantics: everything under the prefix, but not the bare name. */
function isUnderBranchPrefix(ref, prefix) {
  return ref.startsWith(`${prefix}/`) && ref.length > prefix.length + 1;
}

function hasLabel(labels, name) {
  return (labels ?? []).some((label) => label.toLowerCase() === name);
}

/**
 * Why this pull request is not a contributor PR the gate applies to, or null
 * when the gate applies.
 */
export function pullRequestSkipReason(pullRequest, repoFullName) {
  if (hasLabel(pullRequest.labels, TRIVIAL_LABEL)) {
    return `the \`${TRIVIAL_LABEL}\` label is applied`;
  }
  if (sameRepo(pullRequest.headRepoFullName, repoFullName)) {
    const prefix = MAINTAINER_BRANCH_PREFIXES.find((candidate) =>
      isUnderBranchPrefix(pullRequest.headRef ?? "", candidate),
    );
    if (prefix) return `\`${pullRequest.headRef}\` is a maintainer \`${prefix}/**\` branch`;
  }
  return null;
}

/**
 * The whole decision, as a pure function of the pull request, the issue it
 * links (already fetched; null when there is no link or the issue is
 * unreadable), and the event action.
 *
 * @returns {{
 *   conclusion: "success" | "failure",
 *   title: string,
 *   message: string,
 *   linkedIssue: number | null,
 *   convertToDraft: boolean,
 *   skipped: boolean,
 * }}
 */
export function decide({ pullRequest, repoFullName, issue = null, action = "opened" }) {
  const skipReason = pullRequestSkipReason(pullRequest, repoFullName);
  if (skipReason) {
    return {
      conclusion: "success",
      title: "Gate does not apply",
      message: `Design gate skipped: ${skipReason}.`,
      linkedIssue: null,
      convertToDraft: false,
      skipped: true,
    };
  }

  const linked = parseLinkedIssue(pullRequest.body, repoFullName);
  const fail = (title, message) => ({
    conclusion: "failure",
    title,
    message,
    linkedIssue: linked?.number ?? null,
    // A pull request only just marked ready is pushed back to draft; one that
    // was already open stays open and simply fails, so the author is not
    // yanked out from under an in-flight review.
    convertToDraft: action === "ready_for_review",
    skipped: false,
  });

  if (!linked) return fail("No linked issue", GATE_MESSAGE);
  if (!issue) return fail(`Linked issue #${linked.number} is unreadable`, GATE_MESSAGE);
  if (!hasLabel(issue.labels, DESIGN_APPROVED_LABEL)) {
    return fail(
      `Waiting for \`${DESIGN_APPROVED_LABEL}\` on #${issue.number}`,
      `Waiting for \`${DESIGN_APPROVED_LABEL}\` on #${issue.number}.\n\n${GATE_MESSAGE}`,
    );
  }

  return {
    conclusion: "success",
    title: `#${issue.number} is ${DESIGN_APPROVED_LABEL}`,
    message: `#${issue.number} has the \`${DESIGN_APPROVED_LABEL}\` label; this PR can be reviewed.`,
    linkedIssue: issue.number,
    convertToDraft: false,
    skipped: false,
  };
}

/** The note appended when the gate pushed a ready pull request back to draft. */
export function draftNote(linkedIssue) {
  return linkedIssue === null
    ? "Converted back to draft; it will be marked ready automatically once a linked issue is design-approved."
    : `Converted back to draft; it will be marked ready automatically once #${linkedIssue} is design-approved.`;
}

/** Comment body for a decision, or null when no comment should exist yet. */
export function buildCommentBody(decision, { draftConverted = false, commentExists = false } = {}) {
  if (decision.conclusion === "success") {
    // Never open a conversation on a passing PR — only close the one already
    // there, so the author is not left reading a stale blocker.
    return commentExists ? `${COMMENT_MARKER}\n\n${decision.message}` : null;
  }
  const parts = [COMMENT_MARKER, decision.message];
  if (draftConverted) parts.push(draftNote(decision.linkedIssue));
  return parts.join("\n\n");
}

/** Fetch the linked issue (if any) and decide. */
export async function evaluatePullRequest({ api, repoFullName, pullRequest, action = "opened" }) {
  const linked = pullRequestSkipReason(pullRequest, repoFullName)
    ? null
    : parseLinkedIssue(pullRequest.body, repoFullName);
  const issue = linked ? await api.getIssue(linked.number) : null;
  return decide({ pullRequest, repoFullName, issue, action });
}

/**
 * Keep exactly one gate comment per pull request: create it the first time the
 * gate blocks, edit that same comment on every later run.
 */
async function syncComment({ api, pullRequest, decision, draftConverted = false }) {
  const comments = await api.listComments(pullRequest.number);
  const existing = comments.find((comment) => (comment.body ?? "").includes(COMMENT_MARKER));
  const body = buildCommentBody(decision, { draftConverted, commentExists: Boolean(existing) });
  if (body === null) return { action: "none" };
  if (!existing) {
    await api.createComment(pullRequest.number, body);
    return { action: "created" };
  }
  if (existing.body === body) return { action: "unchanged" };
  await api.updateComment(existing.id, body);
  return { action: "updated" };
}

/**
 * Run the gate for one pull request event: decide, push a newly-ready PR back
 * to draft when it fails, and keep the single gate comment in sync.
 *
 * Draft conversion and commenting are best-effort — a fork pull request runs
 * with a read-only token and both will be refused. The returned conclusion
 * never depends on them: a side step that fails must not turn a failing gate
 * green.
 */
export async function runPullRequestGate({
  api,
  repoFullName,
  pullRequest,
  action = "opened",
  log = console,
}) {
  const decision = await evaluatePullRequest({ api, repoFullName, pullRequest, action });

  let draftConverted = false;
  if (decision.convertToDraft && !pullRequest.isDraft) {
    try {
      await api.convertPullRequestToDraft(pullRequest.nodeId);
      draftConverted = true;
    } catch (error) {
      log.warn?.(`design-gate: could not convert #${pullRequest.number} to draft: ${error}`);
    }
  }

  let comment = { action: "none" };
  try {
    comment = await syncComment({ api, pullRequest, decision, draftConverted });
  } catch (error) {
    log.warn?.(`design-gate: could not post the gate comment on #${pullRequest.number}: ${error}`);
  }

  return { ...decision, draftConverted, comment };
}

/**
 * A maintainer labelled an issue `design-approved`: release the draft pull
 * requests that were waiting on it.
 *
 * Marking a PR ready with GITHUB_TOKEN does not start another workflow run, so
 * this arm also publishes the `design-gate` check run itself. Without that the
 * required check would stay red until the author pushed a commit.
 */
export async function runIssueLabeled({ api, repoFullName, issue, log = console }) {
  const candidates = await api.searchDraftPullRequests(issue.number);
  const released = [];

  for (const number of candidates) {
    const pullRequest = await api.getPullRequest(number);
    if (!pullRequest || pullRequest.state !== "open" || !pullRequest.isDraft) continue;

    // The search index matches the raw string `#N` anywhere in the body, so
    // re-parse with the real grammar before touching anything.
    const linked = parseLinkedIssue(pullRequest.body, repoFullName);
    if (!linked || linked.number !== issue.number) continue;

    const decision = decide({ pullRequest, repoFullName, issue, action: "labeled" });
    if (decision.conclusion !== "success") {
      log.warn?.(`design-gate: #${number} still fails the gate; leaving it as a draft`);
      continue;
    }

    await api.markPullRequestReadyForReview(pullRequest.nodeId);
    await api.createCheckRun({
      headSha: pullRequest.headSha,
      conclusion: "success",
      title: decision.title,
      summary: decision.message,
    });
    try {
      await syncComment({ api, pullRequest, decision });
    } catch (error) {
      log.warn?.(`design-gate: could not update the gate comment on #${number}: ${error}`);
    }
    released.push(number);
  }

  return { released };
}

export function normalizePullRequest(raw) {
  return {
    number: raw.number,
    body: raw.body ?? "",
    nodeId: raw.node_id,
    state: raw.state ?? "open",
    isDraft: Boolean(raw.draft),
    labels: (raw.labels ?? []).map((label) => (typeof label === "string" ? label : label.name)),
    headRef: raw.head?.ref ?? "",
    headRepoFullName: raw.head?.repo?.full_name ?? "",
    headSha: raw.head?.sha ?? "",
  };
}

export function normalizeIssue(raw) {
  return {
    number: raw.number,
    state: raw.state ?? "open",
    labels: (raw.labels ?? []).map((label) => (typeof label === "string" ? label : label.name)),
  };
}

/** REST + GraphQL client, narrowed to exactly the calls the gate makes. */
export function createGitHubApi({
  token,
  repoFullName,
  apiBase = process.env.GITHUB_API_URL || "https://api.github.com",
  graphqlUrl = process.env.GITHUB_GRAPHQL_URL || "https://api.github.com/graphql",
  fetchImpl = fetch,
}) {
  const headers = {
    accept: "application/vnd.github+json",
    authorization: `Bearer ${token}`,
    "user-agent": "aft-design-gate",
    "x-github-api-version": "2022-11-28",
  };

  async function rest(method, path, body) {
    const response = await fetchImpl(`${apiBase}${path}`, {
      method,
      headers: body ? { ...headers, "content-type": "application/json" } : headers,
      body: body ? JSON.stringify(body) : undefined,
    });
    if (response.status === 404) return null;
    if (!response.ok) {
      throw new Error(`${method} ${path} failed: ${response.status} ${await response.text()}`);
    }
    return response.status === 204 ? null : await response.json();
  }

  async function graphql(query, variables) {
    const response = await fetchImpl(graphqlUrl, {
      method: "POST",
      headers: { ...headers, "content-type": "application/json" },
      body: JSON.stringify({ query, variables }),
    });
    const payload = await response.json();
    if (!response.ok || payload.errors) {
      throw new Error(`GraphQL failed: ${response.status} ${JSON.stringify(payload.errors ?? {})}`);
    }
    return payload.data;
  }

  return {
    async getIssue(number) {
      const raw = await rest("GET", `/repos/${repoFullName}/issues/${number}`);
      return raw ? normalizeIssue(raw) : null;
    },
    async getPullRequest(number) {
      const raw = await rest("GET", `/repos/${repoFullName}/pulls/${number}`);
      return raw ? normalizePullRequest(raw) : null;
    },
    async listComments(number) {
      const comments = [];
      // Pull request review threads are separate; the gate comment is a plain
      // issue comment, so one paginated pass over that list finds it.
      for (let page = 1; page <= 5; page += 1) {
        const batch = await rest(
          "GET",
          `/repos/${repoFullName}/issues/${number}/comments?per_page=100&page=${page}`,
        );
        if (!batch?.length) break;
        comments.push(...batch);
        if (batch.length < 100) break;
      }
      return comments;
    },
    async createComment(number, body) {
      return await rest("POST", `/repos/${repoFullName}/issues/${number}/comments`, { body });
    },
    async updateComment(id, body) {
      return await rest("PATCH", `/repos/${repoFullName}/issues/comments/${id}`, { body });
    },
    async searchDraftPullRequests(issueNumber) {
      const query = `is:pr is:open draft:true "#${issueNumber}" repo:${repoFullName}`;
      const result = await rest(
        "GET",
        `/search/issues?per_page=100&q=${encodeURIComponent(query)}`,
      );
      return (result?.items ?? []).map((item) => item.number);
    },
    async convertPullRequestToDraft(nodeId) {
      // REST cannot move a pull request back to draft; only GraphQL can.
      await graphql(
        "mutation($id: ID!) { convertPullRequestToDraft(input: { pullRequestId: $id }) { pullRequest { isDraft } } }",
        { id: nodeId },
      );
    },
    async markPullRequestReadyForReview(nodeId) {
      await graphql(
        "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { pullRequest { isDraft } } }",
        { id: nodeId },
      );
    },
    async createCheckRun({ headSha, conclusion, title, summary }) {
      return await rest("POST", `/repos/${repoFullName}/check-runs`, {
        name: CHECK_NAME,
        head_sha: headSha,
        status: "completed",
        conclusion,
        output: { title, summary },
      });
    },
  };
}

function requireEnv(name) {
  const value = process.env[name];
  if (!value) throw new Error(`design-gate: ${name} is not set`);
  return value;
}

function writeStepSummary(text) {
  const path = process.env.GITHUB_STEP_SUMMARY;
  if (path) appendFileSync(path, `${text}\n`);
}

function annotateError(message) {
  const escaped = message.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
  console.log(`::error title=${CHECK_NAME}::${escaped}`);
}

async function main(argv) {
  const mode = argv[0];
  const repoFullName = requireEnv("GITHUB_REPOSITORY");
  const event = JSON.parse(readFileSync(requireEnv("GITHUB_EVENT_PATH"), "utf8"));
  const api = createGitHubApi({ token: requireEnv("GITHUB_TOKEN"), repoFullName });

  if (mode === "pull-request") {
    const pullRequest = normalizePullRequest(event.pull_request);
    const result = await runPullRequestGate({
      api,
      repoFullName,
      pullRequest,
      action: event.action,
    });
    // The job summary is what GitHub shows as the check's summary, so the
    // contributor reads the same words in the checks tab and in the comment.
    writeStepSummary(result.message);
    console.log(`design-gate: #${pullRequest.number} -> ${result.conclusion} (${result.title})`);
    if (result.conclusion !== "success") {
      annotateError(result.message);
      process.exitCode = 1;
    }
    return;
  }

  if (mode === "issue-labeled") {
    const issue = normalizeIssue(event.issue);
    const { released } = await runIssueLabeled({ api, repoFullName, issue });
    const summary = released.length
      ? `Marked ready for review after #${issue.number} was design-approved: ${released
          .map((number) => `#${number}`)
          .join(", ")}`
      : `No draft pull requests were waiting on #${issue.number}.`;
    writeStepSummary(summary);
    console.log(`design-gate: ${summary}`);
    return;
  }

  throw new Error(`design-gate: unknown mode ${JSON.stringify(mode)}`);
}

const invokedPath = process.argv[1] ? pathToFileURL(process.argv[1]).href : "";
if (invokedPath === import.meta.url) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`design-gate: ${error instanceof Error ? error.stack : error}`);
    process.exitCode = 1;
  });
}

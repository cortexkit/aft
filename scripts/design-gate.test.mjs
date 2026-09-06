#!/usr/bin/env node
/**
 * Unit tests for the design-approved gate.
 *
 * Run with: node --test scripts/design-gate.test.mjs
 *
 * Everything here is offline. The GitHub API is a small in-memory fixture, so
 * the suite never touches the real repository — in particular it never creates
 * labels or comments anywhere.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, test } from "node:test";

import {
  buildCommentBody,
  COMMENT_MARKER,
  decide,
  GATE_MESSAGE,
  parseLinkedIssue,
  pullRequestSkipReason,
  runIssueLabeled,
  runPullRequestGate,
} from "./design-gate.mjs";

const REPO = "cortexkit/aft";

function pullRequest(overrides = {}) {
  return {
    number: 100,
    body: "",
    nodeId: "PR_node_100",
    state: "open",
    isDraft: false,
    labels: [],
    headRef: "feature/thing",
    headRepoFullName: "contributor/aft",
    headSha: "a".repeat(40),
    ...overrides,
  };
}

/**
 * In-memory stand-in for the REST + GraphQL calls the gate makes. `refuse`
 * names methods that should throw, which is how a fork pull request's
 * read-only token behaves.
 */
function createFixtureApi({ issues = [], pullRequests = [], comments = [], refuse = [] } = {}) {
  const state = {
    issues: new Map(issues.map((issue) => [issue.number, issue])),
    pullRequests: new Map(pullRequests.map((pr) => [pr.number, pr])),
    comments: comments.map((comment) => ({ ...comment })),
    checkRuns: [],
    readyForReview: [],
    convertedToDraft: [],
    nextCommentId: 1,
  };

  function guard(name) {
    if (refuse.includes(name)) throw new Error(`fixture: ${name} refused (read-only token)`);
  }

  const api = {
    async getIssue(number) {
      return state.issues.get(number) ?? null;
    },
    async getPullRequest(number) {
      return state.pullRequests.get(number) ?? null;
    },
    async listComments(number) {
      guard("listComments");
      return state.comments.filter((comment) => comment.issueNumber === number);
    },
    async createComment(number, body) {
      guard("createComment");
      const comment = { id: state.nextCommentId++, issueNumber: number, body };
      state.comments.push(comment);
      return comment;
    },
    async updateComment(id, body) {
      guard("updateComment");
      const comment = state.comments.find((candidate) => candidate.id === id);
      assert.ok(comment, `fixture: no comment ${id}`);
      comment.body = body;
      return comment;
    },
    async searchDraftPullRequests(issueNumber) {
      // The real search matches the literal string anywhere in the body and
      // knows nothing about closing keywords. Reproduce that looseness so the
      // caller's re-parse is actually exercised.
      return [...state.pullRequests.values()]
        .filter((pr) => pr.state === "open" && pr.isDraft && pr.body.includes(`#${issueNumber}`))
        .map((pr) => pr.number);
    },
    async convertPullRequestToDraft(nodeId) {
      guard("convertPullRequestToDraft");
      state.convertedToDraft.push(nodeId);
      for (const pr of state.pullRequests.values()) if (pr.nodeId === nodeId) pr.isDraft = true;
    },
    async markPullRequestReadyForReview(nodeId) {
      guard("markPullRequestReadyForReview");
      state.readyForReview.push(nodeId);
      for (const pr of state.pullRequests.values()) if (pr.nodeId === nodeId) pr.isDraft = false;
    },
    async createCheckRun(run) {
      guard("createCheckRun");
      state.checkRuns.push(run);
      return run;
    },
  };

  return { api, state };
}

const silentLog = { warn() {}, log() {} };

describe("parseLinkedIssue", () => {
  const cases = [
    ["close #12", 12],
    ["closes #12", 12],
    ["closed #12", 12],
    ["fix #12", 12],
    ["fixes #12", 12],
    ["fixed #12", 12],
    ["resolve #12", 12],
    ["resolves #12", 12],
    ["resolved #12", 12],
    ["CLOSES #12", 12],
    ["Closes: #12", 12],
    ["Closes  #12", 12],
    ["Some prose.\n\nCloses #12\n\nMore prose.", 12],
    ["Closes #12.", 12],
    [`Closes https://github.com/${REPO}/issues/12`, 12],
    [`Closes HTTPS://GITHUB.COM/${REPO}/issues/12`, 12],
    [`Closes ${REPO}#12`, 12],
    // Cross-repository references are somebody else's issue tracker: they are
    // not a link for this gate at all.
    ["Closes https://github.com/other/repo/issues/12", null],
    ["Closes other/repo#12", null],
    // Negative control: a bare reference with no closing keyword is a
    // mention, not a link. GitHub does not close it and neither do we.
    ["See #123 for context", null],
    ["#123", null],
    ["Related to #123", null],
    // Keyword-lookalikes must not match.
    ["Closer #1", null],
    ["Encloses #2", null],
    ["Refixes #3", null],
    // Whitespace between keyword and reference is required.
    ["Closes#12", null],
    ["", null],
    [null, null],
  ];

  for (const [body, expected] of cases) {
    test(`${JSON.stringify(body)} -> ${expected}`, () => {
      const linked = parseLinkedIssue(body, REPO);
      assert.equal(linked?.number ?? null, expected);
    });
  }

  test("takes the first link when several are present", () => {
    assert.equal(parseLinkedIssue("Closes #7\nFixes #9", REPO).number, 7);
  });

  test("skips a cross-repo reference and takes the first same-repo one", () => {
    const body = "Closes https://github.com/other/repo/issues/1\nFixes #9";
    assert.equal(parseLinkedIssue(body, REPO).number, 9);
  });

  test("an unfilled pull request template does not link", () => {
    // Keep in step with .github/pull_request_template.md: a template nobody
    // filled in must fail the gate, not silently satisfy it.
    const template = readFileSync(
      new URL("../.github/pull_request_template.md", import.meta.url),
      "utf8",
    );
    assert.equal(parseLinkedIssue(template, REPO), null);
    assert.equal(parseLinkedIssue(template.replace("Closes #", "Closes #42"), REPO).number, 42);
  });

  test("ignores references GitHub would not linkify", () => {
    assert.equal(parseLinkedIssue("<!-- Closes #5 -->", REPO), null);
    assert.equal(parseLinkedIssue("<!-- Closes #5 -->\nCloses #6", REPO).number, 6);
    assert.equal(parseLinkedIssue("Write `Closes #5` in the description", REPO), null);
    assert.equal(parseLinkedIssue("```\nCloses #5\n```", REPO), null);
  });
});

describe("pullRequestSkipReason", () => {
  const cases = [
    ["maintainer train branch", { headRepoFullName: REPO, headRef: "train/v0.56" }, true],
    ["maintainer alfonso branch", { headRepoFullName: REPO, headRef: "alfonso/task/abc" }, true],
    ["nested train branch", { headRepoFullName: REPO, headRef: "train/a/b/c" }, true],
    // `train/**` is a path prefix, not a substring: a branch simply named
    // `train` or `trainer` is an ordinary branch.
    ["bare train branch name", { headRepoFullName: REPO, headRef: "train" }, false],
    ["trainer branch", { headRepoFullName: REPO, headRef: "trainer" }, false],
    // A fork cannot buy a bypass by naming its branch after a maintainer one.
    ["fork train branch", { headRepoFullName: "contributor/aft", headRef: "train/sneaky" }, false],
    ["fork feature branch", { headRepoFullName: "contributor/aft", headRef: "feature/x" }, false],
    ["maintainer feature branch", { headRepoFullName: REPO, headRef: "feature/x" }, false],
    ["trivial label", { labels: ["trivial"] }, true],
    ["trivial label on a fork", { headRepoFullName: "contributor/aft", labels: ["trivial"] }, true],
    ["unrelated label", { labels: ["bug"] }, false],
  ];

  for (const [name, overrides, skipped] of cases) {
    test(name, () => {
      const reason = pullRequestSkipReason(pullRequest(overrides), REPO);
      assert.equal(reason !== null, skipped, `reason: ${reason}`);
    });
  }
});

describe("decide", () => {
  const approved = { number: 42, state: "open", labels: ["design-approved"] };
  const unapproved = { number: 42, state: "open", labels: ["enhancement"] };

  test("no linked issue fails with the contributor-facing text", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Just a fix" }),
      repoFullName: REPO,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.message, GATE_MESSAGE);
    assert.equal(decision.linkedIssue, null);
  });

  test("linked issue without the label fails and names the issue", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #42" }),
      repoFullName: REPO,
      issue: unapproved,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.linkedIssue, 42);
    assert.ok(decision.message.startsWith("Waiting for `design-approved` on #42."));
    assert.ok(decision.message.includes(GATE_MESSAGE));
  });

  test("linked issue that cannot be read fails closed", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #999" }),
      repoFullName: REPO,
      issue: null,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.message, GATE_MESSAGE);
  });

  test("linked issue with the label passes", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #42" }),
      repoFullName: REPO,
      issue: approved,
    });
    assert.equal(decision.conclusion, "success");
    assert.equal(decision.linkedIssue, 42);
    assert.equal(decision.skipped, false);
  });

  test("skipped pull requests pass without looking at any issue", () => {
    const decision = decide({
      pullRequest: pullRequest({ labels: ["trivial"], body: "no link at all" }),
      repoFullName: REPO,
    });
    assert.equal(decision.conclusion, "success");
    assert.equal(decision.skipped, true);
  });

  const draftCases = [
    ["ready_for_review", "failure-no-issue", "", null, true],
    ["ready_for_review", "failure-unapproved", "Closes #42", unapproved, true],
    ["ready_for_review", "success", "Closes #42", approved, false],
    ["opened", "failure-no-issue", "", null, false],
    ["synchronize", "failure-unapproved", "Closes #42", unapproved, false],
  ];

  for (const [action, name, body, issue, convertToDraft] of draftCases) {
    test(`${action} / ${name} -> convertToDraft=${convertToDraft}`, () => {
      const decision = decide({
        pullRequest: pullRequest({ body }),
        repoFullName: REPO,
        issue,
        action,
      });
      assert.equal(decision.convertToDraft, convertToDraft);
    });
  }
});

describe("contributor-facing text", () => {
  test("is exactly the agreed wording", () => {
    assert.equal(
      GATE_MESSAGE,
      "This PR needs a linked issue with the `design-approved` label before it can be reviewed " +
        "or merged. Add `Closes #<issue>` to the description; a maintainer will apply the label " +
        "on the issue once the design is agreed. Trivial fixes: a maintainer can add the " +
        "`trivial` label to this PR instead.",
    );
  });

  test("a passing pull request with no existing comment gets no comment", () => {
    const decision = { conclusion: "success", message: "fine", linkedIssue: 42 };
    assert.equal(buildCommentBody(decision, { commentExists: false }), null);
    assert.ok(buildCommentBody(decision, { commentExists: true }).startsWith(COMMENT_MARKER));
  });
});

describe("runPullRequestGate", () => {
  test("comments once and edits that same comment on later runs", async () => {
    const { api, state } = createFixtureApi();
    const pr = pullRequest({ body: "No issue here" });

    const first = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pr,
      log: silentLog,
    });
    assert.equal(first.conclusion, "failure");
    assert.equal(first.comment.action, "created");
    assert.equal(state.comments.length, 1);
    assert.ok(state.comments[0].body.includes(COMMENT_MARKER));
    assert.ok(state.comments[0].body.includes(GATE_MESSAGE));

    const second = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pr,
      action: "synchronize",
      log: silentLog,
    });
    assert.equal(second.comment.action, "unchanged");
    assert.equal(state.comments.length, 1);
  });

  test("ready_for_review on a blocked PR converts it back to draft and says so", async () => {
    const pr = pullRequest({ body: "Closes #42" });
    const { api, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: [] }],
      pullRequests: [pr],
    });

    const result = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pr,
      action: "ready_for_review",
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.equal(result.draftConverted, true);
    assert.deepEqual(state.convertedToDraft, [pr.nodeId]);
    assert.ok(
      state.comments[0].body.includes(
        "Converted back to draft; it will be marked ready automatically once #42 is design-approved.",
      ),
    );
  });

  test("a refused draft conversion still fails the gate", async () => {
    const pr = pullRequest({ body: "" });
    const { api, state } = createFixtureApi({
      pullRequests: [pr],
      refuse: ["convertPullRequestToDraft", "listComments", "createComment"],
    });

    const result = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pr,
      action: "ready_for_review",
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.equal(result.draftConverted, false);
    assert.equal(state.comments.length, 0);
    assert.equal(state.convertedToDraft.length, 0);
  });

  test("passing gate leaves a clean PR without a comment", async () => {
    const { api, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: ["design-approved"] }],
    });
    const result = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pullRequest({ body: "Closes #42" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(state.comments.length, 0);
  });

  test("passing gate closes out an existing blocking comment", async () => {
    const { api, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: ["design-approved"] }],
      comments: [{ id: 7, issueNumber: 100, body: `${COMMENT_MARKER}\n\n${GATE_MESSAGE}` }],
    });
    const result = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pullRequest({ body: "Closes #42" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(state.comments.length, 1);
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
    assert.ok(state.comments[0].body.includes("#42 has the `design-approved` label"));
  });

  test("maintainer train branches skip without any API call", async () => {
    const { api, state } = createFixtureApi();
    const result = await runPullRequestGate({
      api,
      repoFullName: REPO,
      pullRequest: pullRequest({ headRepoFullName: REPO, headRef: "train/v0.56", body: "" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(result.skipped, true);
    assert.equal(state.comments.length, 0);
  });
});

describe("runIssueLabeled", () => {
  const labelled = { number: 42, state: "open", labels: ["design-approved"] };

  test("marks the waiting draft ready and publishes a green check", async () => {
    const waiting = pullRequest({
      number: 101,
      nodeId: "PR_node_101",
      isDraft: true,
      body: "Closes #42",
      headSha: "b".repeat(40),
    });
    const { api, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [waiting],
      comments: [{ id: 3, issueNumber: 101, body: `${COMMENT_MARKER}\n\n${GATE_MESSAGE}` }],
    });

    const { released } = await runIssueLabeled({
      api,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, [101]);
    assert.deepEqual(state.readyForReview, ["PR_node_101"]);
    assert.equal(state.pullRequests.get(101).isDraft, false);
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.equal(state.checkRuns[0].headSha, "b".repeat(40));
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
  });

  test("ignores drafts that only mention the issue without a closing keyword", async () => {
    const mention = pullRequest({
      number: 102,
      nodeId: "PR_node_102",
      isDraft: true,
      body: "Follows the discussion in #42 but closes nothing",
    });
    const { api, state } = createFixtureApi({ issues: [labelled], pullRequests: [mention] });

    const { released } = await runIssueLabeled({
      api,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.equal(state.readyForReview.length, 0);
    assert.equal(state.checkRuns.length, 0);
  });

  test("ignores drafts linking a different issue", async () => {
    const other = pullRequest({
      number: 103,
      nodeId: "PR_node_103",
      isDraft: true,
      // The search index matches the literal "#42" in the prose below even
      // though the closing keyword points elsewhere.
      body: "Closes #7, part of the same effort as #42",
    });
    const { api, state } = createFixtureApi({ issues: [labelled], pullRequests: [other] });

    const { released } = await runIssueLabeled({
      api,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.equal(state.readyForReview.length, 0);
  });

  test("leaves already-ready pull requests alone", async () => {
    const ready = pullRequest({
      number: 104,
      nodeId: "PR_node_104",
      isDraft: false,
      body: "Closes #42",
    });
    const { api, state } = createFixtureApi({ issues: [labelled], pullRequests: [ready] });

    const { released } = await runIssueLabeled({
      api,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.equal(state.readyForReview.length, 0);
  });
});

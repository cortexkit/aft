// Independent literal expectations keep the approved wording pinned for both hosts.
export const SHELL_REFUSAL_KNOWN =
  "Permission denied for this shell command: the host denied `git ls-tree HEAD src` (Permission denied: shell). These parts were not denied: `mkdir -p x`. The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

export const SHELL_REFUSAL_UNKNOWN =
  "Permission denied for this shell command: one or more of these parts was denied: `mkdir -p x`, `git ls-tree HEAD src` (Permission denied: shell). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

export const SHELL_REFUSAL_TWO_DENIED =
  "Permission denied for this shell command: the host denied `git ls-tree HEAD src`, `git show HEAD` (Permission denied: shell). These parts were not denied: `mkdir -p x`. The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

export const SHELL_REFUSAL_ALL_DENIED =
  "Permission denied for this shell command: the host denied `mkdir -p x`, `git ls-tree HEAD src` (Permission denied: shell). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

//! Credential masking for durable logs and background completion previews.
//!
//! AFT log lines can quote URLs, error text from child processes and LSP
//! servers, and other strings that occasionally carry a credential. Every line
//! bound for the log file passes through [`aft_redactor`] once before it is
//! written (see `logging::TeeWriter`).
//!
//! Shared token and URL patterns are also exercised by the CLI sanitizer in
//! `packages/aft-cli/src/lib/sanitize.ts` through
//! `tests/fixtures/log_redaction_cases.json`. The durable sink additionally masks
//! contextual secret fields and private-key records before shortening them.
//!
//! What is masked:
//! - GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`),
//!   `sk-` API keys, JWTs, AWS access key IDs, and `Authorization: Bearer|Basic`
//!   header values;
//! - URL userinfo (`https://user:pass@host` becomes `https://***@host`);
//! - credential query parameters (`token=`, `access_token=`, `api_key=`,
//!   `…secret=`, `password=`), and `key=` when its value looks like a secret.
//!
//! Deliberately NOT masked: bare `key=value` fields outside a URL query. AFT's
//! own structured lines use `key=` for index keys, and masking them would
//! destroy the forensic value of the log.
//!
//! Shell commands quoted in a log line additionally go through
//! [`redact_command`], which masks credentials typed inline in a command
//! (`GITHUB_TOKEN=...`, `--password ...`). Those shapes are command-only for
//! the reason above.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex, Replacer};

/// Replacement for a token-shaped secret. Matches the CLI sanitizer.
pub const SECRET_PLACEHOLDER: &str = "<REDACTED_SECRET>";
/// Replacement for URL userinfo. Matches the CLI sanitizer.
pub const URL_CREDENTIAL_PLACEHOLDER: &str = "***";

/// A `key=` query value shorter than this is treated as an ordinary word.
const KEY_VALUE_MIN_SECRET_LEN: usize = 16;

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("log redaction pattern must compile")
}

static AUTHORIZATION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)\b((?:proxy-)?authorization[^\S\r\n]*:[^\S\r\n]*(?:bearer|basic|token)[^\S\r\n]+)[A-Za-z0-9._~+/-]+=*",
    )
});
static API_KEY_HEADER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(x-api-key[^\S\r\n]*:[^\S\r\n]*)[^\s<>]+"));
static SECRET_JSON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r#"(?i)("(?:{JSON_SECRET_LABELS})"\s*:\s*")((?:\\.|[^"\\])+)(")"#
    ))
});
/// The opening of a [`SECRET_JSON_FIELD`] value, up to and including its quote.
static SECRET_JSON_OPENING: LazyLock<Regex> =
    LazyLock::new(|| compile(&format!(r#"(?i)"(?:{JSON_SECRET_LABELS})"\s*:\s*""#)));
const JSON_SECRET_LABELS: &str = "password|passwd|pwd|api[_-]?key|apikey|x-api-key|aws_secret_access_key|awsSecretAccessKey|secret_access_key";
static AWS_SECRET_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)\b((?:aws_secret_access_key|secret_access_key)[^\S\r\n]*[=:][^\S\r\n]*)[^\s"'<>]+"#,
    )
});
static SLACK_TOKEN: LazyLock<Regex> = LazyLock::new(|| compile(r"\bxox[baprs]-[A-Za-z0-9-]+\b"));
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
});

static GITHUB_PAT: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgithub_pat_[A-Za-z0-9_]+\b"));
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\bgh[pousr]_[A-Za-z0-9_]{16,}\b"));
static SK_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"\bsk-(?:live-[A-Za-z0-9][A-Za-z0-9_-]{7,}|[A-Za-z0-9][A-Za-z0-9_-]{19,})\b")
});
static JWT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b"));
static AWS_ACCESS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA)[A-Z0-9]{16}\b"));
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b([a-z][a-z0-9+.-]*://)[^@\s/?#]+@"));
/// Query parameters whose name alone says the value is a credential. The value
/// stops at the next parameter, fragment, whitespace, quote or angle bracket;
/// excluding `<` keeps an already-masked value from matching again.
static CREDENTIAL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)([?&](?:[a-z0-9_-]*token|api[_-]?key|apikey|[a-z0-9_-]*secret|password|passwd|pwd)=)[^&\s#"'<>]+"#,
    )
});
/// `key=` is too generic to mask on its name alone; see [`looks_like_secret`].
static KEY_QUERY: LazyLock<Regex> = LazyLock::new(|| compile(r#"(?i)([?&]key=)([^&\s#"'<>]+)"#));

/// Mask credentials in `text`. Returns the input unchanged (borrowed) when it
/// contains nothing to mask, so the common case allocates nothing.
pub fn aft_redactor(text: &str) -> Cow<'_, str> {
    redact_with_range(text, None)
}

// Shell-command credential shapes. These are applied only to command text
// (see [`redact_command`]), not to every log line: AFT's own structured log
// fields are lowercase `key=value` pairs and must stay readable.
//
// The leading `(^|[^...])` group stands in for a look-behind, which the regex
// crate lacks; replacements re-emit it.

/// One whole shell word, as the shell would read it: single-quoted runs,
/// double-quoted runs with backslash escapes, backslash-escaped characters
/// and plain characters, concatenated (`'a'"b"c`, `my\ pass`), up to the
/// next unquoted whitespace or shell operator. A quote left open runs to the
/// end of the text. `<` ends a word, so an already-masked value is never
/// matched again.
macro_rules! shell_word {
    () => {
        r#"(?:'[^']*'?|"(?:\\[\s\S]|[^"\\])*"?|\\[\s\S]?|[^\s;&|<>()'"\\])+"#
    };
}

/// `NAME=value` where NAME is UPPER_SNAKE and contains a credential word,
/// at its start or after other letters, followed by `=` or `_` (`TOKEN=`,
/// `GITHUB_TOKEN=`, `export DB_PASSWORD=`, `AWS_SECRET_ACCESS_KEY_ID=`).
/// Plain substrings do not count: `MAX_TOKENS`, `TOKENIZERS_PARALLELISM`,
/// `GIT_AUTHOR_NAME` and `GH_SHIM_BYPASS` stay.
static COMMAND_SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r#"(^|[^A-Za-z0-9_])((?:[A-Z][A-Z0-9_]*?)?(?:TOKEN|SECRET|PASSWORD|PASSWD|PASSPHRASE|CREDENTIALS?|APIKEY|API_KEY|ACCESS_KEY|PRIVATE_KEY|AUTH_KEY|_KEY|_PAT|_AUTH)(?:_[A-Z0-9]+)*=)("#,
        shell_word!(),
        ")"
    ))
});
/// Assignment names ending in one of these name where a credential lives or
/// who owns it, not the credential (`SSH_KEY_PATH=`, `AWS_ACCESS_KEY_ID=`).
const COMMAND_NON_SECRET_NAME_SUFFIXES: [&str; 16] = [
    "_PATH=",
    "_FILE=",
    "_DIR=",
    "_NAME=",
    "_URL=",
    "_ID=",
    "_HOST=",
    "_PORT=",
    "_USER=",
    "_USERNAME=",
    "_ENV=",
    "_COUNT=",
    "_LEN=",
    "_LENGTH=",
    "_TTL=",
    "_MODE=",
];
/// Long options whose value is a credential, as `--flag=value` or
/// `--flag value`. `--secret` is deliberately absent: gcloud and others use it
/// for a secret's name.
static COMMAND_SECRET_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r#"(^|[^A-Za-z0-9_-])(--(?:password|passwd|pass|token|api-key|apikey|client-secret|auth-token|access-token|bearer)(?:=|[ \t]+))("#,
        shell_word!(),
        ")"
    ))
});
/// How deep [`mask_tool_passwords`] looks into quoted strings that hold a
/// command of their own (`bash -c "curl -u admin:pw ..."`).
const QUOTED_COMMAND_DEPTH: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellTokenKind {
    Word,
    /// A redirection with its target (`<f`, `> f`, `2>&1`, `<<<word`). It is
    /// neither an argument nor the value of the option before it.
    Redirect,
}

#[derive(Clone)]
struct ShellToken {
    kind: ShellTokenKind,
    range: std::ops::Range<usize>,
}

/// Split `text` into simple commands of words and redirections, reading it
/// as the shell would: quotes and backslashes keep operators inside a word;
/// an unquoted `;`, `&`, `|`, newline, `(`, `)` or backtick ends a command
/// (so subshells, `$( ... )` and backtick substitutions are commands of
/// their own); a backslash-newline continues the line; redirections may be
/// attached to a word or spaced from it. Also returns the inside of every
/// quoted run, which may hold a command for another shell.
fn split_shell_commands(text: &str) -> (Vec<Vec<ShellToken>>, Vec<std::ops::Range<usize>>) {
    let bytes = text.as_bytes();
    let mut commands = vec![Vec::new()];
    let mut quoted = Vec::new();
    let end_command = |commands: &mut Vec<Vec<ShellToken>>| {
        if !commands.last().is_some_and(Vec::is_empty) {
            commands.push(Vec::new());
        }
    };
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'\r' => at += 1,
            b'\\' if bytes.get(at + 1) == Some(&b'\n') => at += 2,
            b'\n' | b';' | b'(' | b')' | b'`' => {
                end_command(&mut commands);
                at += 1;
            }
            b'|' => {
                end_command(&mut commands);
                at += 1 + usize::from(matches!(bytes.get(at + 1), Some(b'|' | b'&')));
            }
            b'&' if bytes.get(at + 1) != Some(&b'>') => {
                end_command(&mut commands);
                at += 1 + usize::from(bytes.get(at + 1) == Some(&b'&'));
            }
            b'&' | b'<' | b'>' => {
                let (token, next) = scan_redirect(text, at, &mut quoted);
                commands.last_mut().unwrap().push(token);
                at = next;
            }
            _ => {
                let next = scan_shell_word(text, at, &mut quoted);
                // A file-descriptor number attached to a redirection (`2>`).
                if text[at..next].bytes().all(|byte| byte.is_ascii_digit())
                    && matches!(bytes.get(next), Some(b'<' | b'>'))
                {
                    let (mut token, after) = scan_redirect(text, next, &mut quoted);
                    token.range.start = at;
                    commands.last_mut().unwrap().push(token);
                    at = after;
                } else {
                    commands.last_mut().unwrap().push(ShellToken {
                        kind: ShellTokenKind::Word,
                        range: at..next,
                    });
                    at = next;
                }
            }
        }
    }
    (commands, quoted)
}

/// The end of the shell word starting at `start`, recording the inside of
/// each quoted run. An unclosed quote runs to the end of `text`.
fn scan_shell_word(text: &str, start: usize, quoted: &mut Vec<std::ops::Range<usize>>) -> usize {
    let bytes = text.as_bytes();
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => {
                let close = text[at + 1..]
                    .find('\'')
                    .map_or(bytes.len(), |off| at + 1 + off);
                quoted.push(at + 1..close);
                at = (close + 1).min(bytes.len());
            }
            b'"' => {
                let mut close = at + 1;
                while close < bytes.len() && bytes[close] != b'"' {
                    close += if bytes[close] == b'\\' { 2 } else { 1 };
                }
                let close = close.min(bytes.len());
                quoted.push(at + 1..close);
                at = (close + 1).min(bytes.len());
            }
            b'\\' => at = (at + 2).min(bytes.len()),
            byte if byte.is_ascii_whitespace()
                || matches!(byte, b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'`') =>
            {
                break
            }
            _ => at += 1,
        }
    }
    // Stay on a character boundary after a backslash before a multibyte char.
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// A redirection operator at `start` (`<`, `>>`, `<<<`, `>&`, `&>`, `>|`,
/// `<<-`) and its target word, attached or after spaces, if one follows.
fn scan_redirect(
    text: &str,
    start: usize,
    quoted: &mut Vec<std::ops::Range<usize>>,
) -> (ShellToken, usize) {
    let bytes = text.as_bytes();
    let mut at = start + usize::from(bytes[start] == b'&');
    let mut arrows = 0;
    while arrows < 3 && matches!(bytes.get(at), Some(b'<' | b'>')) {
        at += 1;
        arrows += 1;
    }
    if matches!(bytes.get(at), Some(b'&' | b'|' | b'-')) {
        at += 1;
    }
    let mut target = at;
    while matches!(bytes.get(target), Some(b' ' | b'\t')) {
        target += 1;
    }
    let starts_word = bytes.get(target).is_some_and(|byte| {
        !byte.is_ascii_whitespace()
            && !matches!(byte, b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'`')
    });
    if starts_word {
        at = scan_shell_word(text, target, quoted);
    }
    (
        ShellToken {
            kind: ShellTokenKind::Redirect,
            range: start..at,
        },
        at,
    )
}

/// A shell word as the program sees it: quotes removed, escapes resolved.
fn unquote_shell_word(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(open), _) if ch == open => quote = None,
            (Some('\''), _) => out.push(ch),
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        out.push(next);
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Mask the passwords that curl (`-u`/`--user`), the MySQL family (`-p<pw>`)
/// and sshpass (`-p pw`) take on the command line, wherever those programs
/// run in `command`: after assignments or wrappers (`env`, `sudo`,
/// `command`), inside subshells and substitutions, across continuation
/// lines, and inside a quoted string handed to another shell.
fn mask_tool_passwords(command: &str) -> Cow<'_, str> {
    if !["curl", "mysql", "mariadb", "sshpass"]
        .iter()
        .any(|program| command.contains(program))
    {
        return Cow::Borrowed(command);
    }
    let mut masks = Vec::new();
    collect_tool_password_masks(command, 0, 0, &mut masks);
    if masks.is_empty() {
        return Cow::Borrowed(command);
    }
    masks.sort_by_key(|(range, _): &(std::ops::Range<usize>, String)| range.start);
    let mut out = String::with_capacity(command.len());
    let mut copied = 0;
    for (range, replacement) in masks {
        if range.start < copied {
            continue;
        }
        out.push_str(&command[copied..range.start]);
        out.push_str(&replacement);
        copied = range.end;
    }
    out.push_str(&command[copied..]);
    if out == command {
        Cow::Borrowed(command)
    } else {
        Cow::Owned(out)
    }
}

#[cfg(test)]
thread_local! {
    /// Bytes tokenized plus tokens visited by the tool-password scan, so a
    /// test can check the work stays linear in the command's length.
    static SHELL_SCAN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// A test's step budget: exceeding it panics at once, so a superlinear
    /// scan fails quickly instead of running for hours on a large command.
    static SHELL_SCAN_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

#[cfg(test)]
fn note_shell_scan_steps(steps: usize) {
    let total = SHELL_SCAN_STEPS.with(|count| {
        count.set(count.get().saturating_add(steps));
        count.get()
    });
    let budget = SHELL_SCAN_BUDGET.with(std::cell::Cell::get);
    assert!(
        total <= budget,
        "shell scan took {total} steps, over its budget of {budget}"
    );
}

#[cfg(not(test))]
fn note_shell_scan_steps(_steps: usize) {}

/// Where sshpass's own options stand in one command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SshpassOptions {
    NotSeen,
    Reading,
    /// The next word is the value of `-f`, `-d` or `-P`.
    SkipValue,
    /// The next word is the password given to `-p`.
    PasswordNext,
    /// The wrapped command has started; its `-p` is not sshpass's.
    Done,
}

/// Find the tool passwords in `text` (one level of quoting) in a single pass
/// over each command's tokens, then in the quoted runs, at most
/// [`QUOTED_COMMAND_DEPTH`] levels down. Quoted runs at one level do not
/// overlap, so each byte is tokenized at most once per level.
fn collect_tool_password_masks(
    text: &str,
    base: usize,
    depth: usize,
    masks: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    let (commands, quoted) = split_shell_commands(text);
    note_shell_scan_steps(text.len() + 1);
    let mut local = Vec::new();
    for tokens in &commands {
        note_shell_scan_steps(tokens.len());
        // A program's arguments run to the end of its command, as the
        // shell passes them; each program seen so far keeps reading them.
        let (mut curl, mut mysql) = (false, false);
        let mut curl_user_next = false;
        let mut sshpass = SshpassOptions::NotSeen;
        for token in tokens {
            if token.kind == ShellTokenKind::Redirect {
                // An option followed by a redirection has no value here.
                curl_user_next = false;
                if sshpass == SshpassOptions::PasswordNext || sshpass == SshpassOptions::SkipValue {
                    sshpass = SshpassOptions::Done;
                }
                continue;
            }
            let raw = &text[token.range.clone()];
            if curl_user_next {
                curl_user_next = false;
                if let Some(masked) = mask_curl_password(raw) {
                    local.push((token.range.clone(), masked));
                }
                continue;
            }
            match sshpass {
                SshpassOptions::PasswordNext => {
                    if !command_value_is_reference(raw) {
                        local.push((token.range.clone(), SECRET_PLACEHOLDER.to_string()));
                    }
                    sshpass = SshpassOptions::Done;
                    continue;
                }
                SshpassOptions::SkipValue => {
                    sshpass = SshpassOptions::Reading;
                    continue;
                }
                _ => {}
            }
            let plain = unquote_shell_word(raw);
            let program = plain.rsplit('/').next().unwrap_or_default();
            let program = program.strip_suffix(".exe").unwrap_or(program);
            let is_curl = program == "curl";
            let is_mysql = matches!(program, "mysql" | "mariadb" | "mysqladmin" | "mysqldump");
            if (is_curl || is_mysql) && sshpass == SshpassOptions::Reading {
                // The command sshpass wraps has started.
                sshpass = SshpassOptions::Done;
            }
            match program {
                _ if is_curl => {
                    curl = true;
                    continue;
                }
                _ if is_mysql => {
                    mysql = true;
                    continue;
                }
                "sshpass" => {
                    sshpass = SshpassOptions::Reading;
                    continue;
                }
                _ => {}
            }
            if curl {
                if plain == "-u" || plain == "--user" {
                    curl_user_next = true;
                    continue;
                }
                if plain.starts_with("--user=") {
                    curl_attached_mask(text, token, "--user=", &mut local);
                } else if plain.starts_with("-u") && !plain.starts_with("--") {
                    curl_attached_mask(text, token, "-u", &mut local);
                }
            }
            if mysql && plain.starts_with("-p") {
                mask_attached_password(text, token, "-p", &mut local);
            }
            if sshpass == SshpassOptions::Reading {
                sshpass = match plain.as_str() {
                    "-p" => SshpassOptions::PasswordNext,
                    "-f" | "-d" | "-P" => SshpassOptions::SkipValue,
                    _ if plain.starts_with("-p") => {
                        mask_attached_password(text, token, "-p", &mut local);
                        SshpassOptions::Done
                    }
                    _ if plain.starts_with('-') => SshpassOptions::Reading,
                    _ => SshpassOptions::Done,
                };
            }
        }
    }
    masks.extend(
        local
            .into_iter()
            .map(|(range, replacement)| (range.start + base..range.end + base, replacement)),
    );
    if depth < QUOTED_COMMAND_DEPTH {
        for inner in quoted {
            collect_tool_password_masks(&text[inner.clone()], base + inner.start, depth + 1, masks);
        }
    }
}

/// The value attached to an option word (`-uadmin:pw`, `--user=admin:pw`,
/// `-ppw`) after `prefix`, as a range of `text`, when the word starts with
/// the option as typed. A word whose option part is itself quoted
/// (`"-ppw"`) yields `None` and is masked whole by the caller.
fn attached_option_value(
    text: &str,
    token: &ShellToken,
    prefix: &str,
) -> Option<std::ops::Range<usize>> {
    let raw = &text[token.range.clone()];
    (raw.len() > prefix.len() && raw.starts_with(prefix))
        .then(|| token.range.start + prefix.len()..token.range.end)
}

/// Mask the password half of an attached curl user value. When the option
/// itself is quoted, the whole word becomes the placeholder: every byte that
/// stays in the log is then one the command really had.
fn curl_attached_mask(
    text: &str,
    token: &ShellToken,
    prefix: &str,
    masks: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    match attached_option_value(text, token, prefix) {
        Some(range) => {
            if let Some(masked) = mask_curl_password(&text[range.clone()]) {
                masks.push((range, masked));
            }
        }
        None => {
            let plain = unquote_shell_word(&text[token.range.clone()]);
            let value = plain.strip_prefix(prefix).unwrap_or_default();
            if mask_curl_password(value).is_some() {
                masks.push((token.range.clone(), SECRET_PLACEHOLDER.to_string()));
            }
        }
    }
}

/// Push a placeholder over an attached option value unless it is a
/// reference; a quoted option word is masked whole.
fn mask_attached_password(
    text: &str,
    token: &ShellToken,
    prefix: &str,
    masks: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    match attached_option_value(text, token, prefix) {
        Some(range) => {
            if !command_value_is_reference(&text[range.clone()]) {
                masks.push((range, SECRET_PLACEHOLDER.to_string()));
            }
        }
        None => {
            let plain = unquote_shell_word(&text[token.range.clone()]);
            let value = plain.strip_prefix(prefix).unwrap_or_default();
            if !command_value_is_reference(value) {
                masks.push((token.range.clone(), SECRET_PLACEHOLDER.to_string()));
            }
        }
    }
}

/// Mask what follows the first `:` of a `user:password` word, keeping the
/// user and a quote pair that wraps the whole word (`"admin:<masked>"`).
/// A word without a password, or whose password is a reference, is kept.
fn mask_curl_password(word: &str) -> Option<String> {
    let quote = word
        .chars()
        .next()
        .filter(|first| matches!(first, '"' | '\'') && word.len() > 1 && word.ends_with(*first));
    let body = match quote {
        Some(_) => &word[1..word.len() - 1],
        None => word,
    };
    let (user, password) = body.split_once(':')?;
    if command_value_is_reference(password) {
        return None;
    }
    let quote = quote.map(String::from).unwrap_or_default();
    Some(format!("{quote}{user}:{SECRET_PLACEHOLDER}{quote}"))
}

/// A value that names another value instead of holding one: a variable or
/// command substitution (`$GH_TOKEN`, `"$(gcloud secrets ...)"`), an empty
/// string, a placeholder this module already wrote, or the next option.
fn command_value_is_reference(value: &str) -> bool {
    let unquoted = value.trim_matches(|ch| ch == '"' || ch == '\'');
    unquoted.is_empty() || unquoted.starts_with(['$', '<', '-'])
}

/// Mask credentials typed inline in a shell command, for logs that quote the
/// command. Applies [`aft_redactor`] plus command-only shapes: secret-named
/// `NAME=value` assignments (including after `export` or `env`), credential
/// long options, MySQL's `-p<value>`, `sshpass -p` and `curl -u user:pass`.
/// Mask the whole command before shortening it for a log line: a token cut
/// short no longer matches its pattern.
pub fn redact_command(command: &str) -> Cow<'_, str> {
    // Runs first, on the command as typed: the shapes below write
    // placeholders that read as redirections to the shell-word scan.
    let mut out = mask_tool_passwords(command);
    if command.contains('=') {
        out = replace_in(
            out,
            None,
            &COMMAND_SECRET_ASSIGNMENT,
            |caps: &Captures<'_>| {
                let name = &caps[2];
                if command_value_is_reference(&caps[3])
                    || COMMAND_NON_SECRET_NAME_SUFFIXES
                        .iter()
                        .any(|suffix| name.ends_with(suffix))
                {
                    caps[0].to_string()
                } else {
                    format!("{}{name}{SECRET_PLACEHOLDER}", &caps[1])
                }
            },
        );
    }
    if command.contains("--") {
        out = replace_in(out, None, &COMMAND_SECRET_FLAG, |caps: &Captures<'_>| {
            if command_value_is_reference(&caps[3]) {
                caps[0].to_string()
            } else {
                format!("{}{}{SECRET_PLACEHOLDER}", &caps[1], &caps[2])
            }
        });
    }
    match aft_redactor(&out) {
        Cow::Borrowed(_) => out,
        Cow::Owned(masked) => Cow::Owned(masked),
    }
}

// Separate limits bound byte scanning and expensive candidate hashes. Long
// non-hex verdict lines need no hashes and can still retain their placeholders.
pub(crate) const TICKET_REDACTION_BYTES: usize = 50 * 1024;
pub(crate) const TICKET_REDACTION_WINDOWS: usize = 8 * 1024;
pub(crate) const TICKET_HALO_BYTES: usize = 31;

#[derive(Debug)]
pub(crate) struct TicketRedactionBudgetExceeded;

/// Redact only a 32-hex candidate whose BLAKE3 digest matches this task's stored
/// ticket digest. This keeps ordinary hashes and identifiers unchanged in previews.
pub(crate) fn redact_ticket_digest<'a>(
    text: &'a str,
    digest: Option<&str>,
) -> Result<Cow<'a, str>, TicketRedactionBudgetExceeded> {
    redact_ticket_digest_with_range(text, None, digest)
}

#[cfg(test)]
thread_local! {
    pub(crate) static TICKET_HASH_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

/// Range-aware form used when a watch match is anchored in captured output.
pub(crate) fn redact_ticket_digest_range<'a>(
    text: &'a str,
    range: &mut std::ops::Range<usize>,
    digest: Option<&str>,
) -> Result<Cow<'a, str>, TicketRedactionBudgetExceeded> {
    redact_ticket_digest_with_range(text, Some(range), digest)
}

fn redact_ticket_digest_with_range<'a>(
    text: &'a str,
    mut range: Option<&mut std::ops::Range<usize>>,
    digest: Option<&str>,
) -> Result<Cow<'a, str>, TicketRedactionBudgetExceeded> {
    let Some(digest) = digest else {
        return Ok(Cow::Borrowed(text));
    };
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(Cow::Borrowed(text));
    }
    // Refuse before hashing any candidate. A caller must withhold the rendered
    // region rather than fall back to the unmasked input on budget exhaustion.
    if text.len() > TICKET_REDACTION_BYTES || ticket_digest_windows(text) > TICKET_REDACTION_WINDOWS
    {
        return Err(TicketRedactionBudgetExceeded);
    }
    let bytes = text.as_bytes();
    if bytes.len() < 32 {
        return Ok(Cow::Borrowed(text));
    }
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !bytes[cursor].is_ascii_hexdigit() {
            cursor += 1;
            continue;
        }
        let run_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_hexdigit() {
            cursor += 1;
        }
        let run_end = cursor;
        if run_end - run_start < 32 {
            continue;
        }
        for start in run_start..=(run_end - 32) {
            let end = start + 32;
            let candidate = &text[start..end];
            #[cfg(test)]
            TICKET_HASH_PROBE.with(|probe| {
                if let Some(probe) = probe.borrow_mut().as_mut() {
                    probe();
                }
            });
            if blake3::hash(candidate.as_bytes()).to_hex().as_str() != digest {
                continue;
            }
            if let Some(previous) = spans.last_mut() {
                if start < previous.end {
                    previous.end = previous.end.max(end);
                    continue;
                }
            }
            spans.push(start..end);
        }
    }
    if spans.is_empty() {
        return Ok(Cow::Borrowed(text));
    }
    let mut masked = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        masked.push_str(&text[copied..span.start]);
        let replacement_start = masked.len();
        masked.push_str(SECRET_PLACEHOLDER);
        if let Some(range) = range.as_deref_mut() {
            map_redacted_range(
                range,
                replacement_start,
                replacement_start + span.len(),
                SECRET_PLACEHOLDER.len(),
            );
        }
        copied = span.end;
    }
    masked.push_str(&text[copied..]);
    Ok(Cow::Owned(masked))
}

pub(crate) fn ticket_digest_windows(text: &str) -> usize {
    text.as_bytes()
        .split(|byte| !byte.is_ascii_hexdigit())
        .map(|run| run.len().saturating_sub(31))
        .sum()
}

fn redact_with_range<'a>(
    text: &'a str,
    mut range: Option<&mut std::ops::Range<usize>>,
) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    if text.contains("PRIVATE KEY-----") {
        out = replace_in(out, range.as_deref_mut(), &PRIVATE_KEY, SECRET_PLACEHOLDER);
    }
    if text.contains("xox") {
        out = replace_in(out, range.as_deref_mut(), &SLACK_TOKEN, SECRET_PLACEHOLDER);
    }
    if contains_ignore_ascii_case(text, "x-api-key") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &API_KEY_HEADER,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if text.contains('"') {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &SECRET_JSON_FIELD,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}{}", &caps[1], &caps[3]),
        );
    }
    if contains_ignore_ascii_case(text, "secret_access_key") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AWS_SECRET_FIELD,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if contains_ignore_ascii_case(text, "authorization") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AUTHORIZATION_HEADER,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
    }
    if text.contains("github_pat_") {
        out = replace_in(out, range.as_deref_mut(), &GITHUB_PAT, SECRET_PLACEHOLDER);
    }
    if needs_github_token_scan(text) {
        out = replace_in(out, range.as_deref_mut(), &GITHUB_TOKEN, SECRET_PLACEHOLDER);
    }
    if text.contains("sk-") {
        out = replace_in(out, range.as_deref_mut(), &SK_API_KEY, SECRET_PLACEHOLDER);
    }
    if text.contains("eyJ") {
        out = replace_in(out, range.as_deref_mut(), &JWT, SECRET_PLACEHOLDER);
    }
    if ["AKIA", "ASIA", "AGPA", "AIDA", "AROA"]
        .iter()
        .any(|prefix| text.contains(prefix))
    {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &AWS_ACCESS_KEY_ID,
            SECRET_PLACEHOLDER,
        );
    }
    if text.contains("://") {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &URL_USERINFO,
            |caps: &Captures<'_>| format!("{}{URL_CREDENTIAL_PLACEHOLDER}@", &caps[1]),
        );
    }
    if text.contains('=') && (text.contains('?') || text.contains('&')) {
        out = replace_in(
            out,
            range.as_deref_mut(),
            &CREDENTIAL_QUERY,
            |caps: &Captures<'_>| format!("{}{SECRET_PLACEHOLDER}", &caps[1]),
        );
        out = replace_in(
            out,
            range.as_deref_mut(),
            &KEY_QUERY,
            |caps: &Captures<'_>| {
                if looks_like_secret(&caps[2]) {
                    format!("{}{SECRET_PLACEHOLDER}", &caps[1])
                } else {
                    caps[0].to_string()
                }
            },
        );
    }
    out
}

/// A command's credential values, retained only with its live in-memory task.
/// Deliberately has no Debug or serialization implementation.
#[derive(Default)]
pub(crate) struct SecretValues(Vec<String>);

/// Whether an environment variable name marks its value as a completion secret.
pub fn is_secret_environment_key(key: &str) -> bool {
    if key.eq_ignore_ascii_case(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV) {
        return true;
    }
    // Git's numbered config variables carry a setting NAME in GIT_CONFIG_KEY_<n>
    // (AFT itself injects core.hooksPath this way). The name is not a secret,
    // and dropping it alone while GIT_CONFIG_COUNT and GIT_CONFIG_VALUE_<n>
    // stay makes every git child exit with "missing config key".
    if key
        .get(.."GIT_CONFIG_KEY_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GIT_CONFIG_KEY_"))
    {
        return false;
    }
    [
        "TOKEN",
        "SECRET",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|part| contains_ignore_ascii_case(key, part))
}

impl SecretValues {
    pub(crate) fn longest_value_len(&self) -> usize {
        self.0.iter().map(String::len).max().unwrap_or(0)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Byte ranges in `text` of every occurrence of a value that contains a
    /// newline. Only these occurrences can cross a cut made at a newline.
    pub(crate) fn cross_line_spans(&self, text: &str) -> Vec<std::ops::Range<usize>> {
        let mut spans = Vec::new();
        for value in self.0.iter().filter(|value| value.contains('\n')) {
            let mut start = 0;
            while let Some(offset) = text[start..].find(value.as_str()) {
                let from = start + offset;
                spans.push(from..from + value.len());
                start = from + text[from..].chars().next().unwrap().len_utf8();
            }
        }
        spans
    }

    pub(crate) fn from_environment<'a>(
        env: impl IntoIterator<Item = (&'a std::ffi::OsStr, &'a std::ffi::OsStr)>,
    ) -> Self {
        let mut values = Vec::new();
        for (key, value) in env {
            let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
                continue;
            };
            if value.chars().count() >= 8 && is_secret_environment_key(key) {
                values.push(value.to_owned());
            }
        }
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self(values)
    }

    pub(crate) fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.redact_range(text, None)
    }

    pub(crate) fn redact_range<'a>(
        &self,
        text: &'a str,
        mut range: Option<&mut std::ops::Range<usize>>,
    ) -> Cow<'a, str> {
        // Exact matches run first: a format-based replacement inside an opaque
        // value must not leave the rest of that value visible.
        let mut spans = Vec::new();
        for value in &self.0 {
            let mut start = 0;
            while let Some(offset) = text[start..].find(value) {
                let from = start + offset;
                spans.push((from, from + value.len()));
                start = from + text[from..].chars().next().unwrap().len_utf8();
            }
        }
        let out = if spans.is_empty() {
            Cow::Borrowed(text)
        } else {
            spans.sort_unstable();
            let mut masked = String::new();
            let mut copied = 0;
            let mut spans = spans.into_iter().peekable();
            while let Some((start, mut end)) = spans.next() {
                while spans.peek().is_some_and(|(next, _)| *next <= end) {
                    end = end.max(spans.next().unwrap().1);
                }
                masked.push_str(&text[copied..start]);
                if let Some(range) = range.as_deref_mut() {
                    map_redacted_range(
                        range,
                        masked.len(),
                        masked.len() + end - start,
                        SECRET_PLACEHOLDER.len(),
                    );
                }
                masked.push_str(SECRET_PLACEHOLDER);
                copied = end;
            }
            masked.push_str(&text[copied..]);
            Cow::Owned(masked)
        };
        match redact_with_range(&out, range) {
            Cow::Borrowed(_) => out,
            Cow::Owned(masked) => Cow::Owned(masked),
        }
    }
}

fn needs_github_token_scan(text: &str) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| text.contains(prefix))
}

// How far a match can reach. A caller that masks a slice of a larger capture
// (a watch preview) cuts that slice at newlines. The cut is safe for a pattern
// only if none of its matches can contain a newline:
// - AUTHORIZATION_HEADER, API_KEY_HEADER and AWS_SECRET_FIELD separate their
//   parts with `[^\S\r\n]` (spaces and tabs only) and their values exclude `\s`;
// - SLACK_TOKEN, GITHUB_PAT, GITHUB_TOKEN, SK_API_KEY, JWT and
//   AWS_ACCESS_KEY_ID consist of letters, digits, `_`, `-` and `.` only;
// - URL_USERINFO, CREDENTIAL_QUERY and KEY_QUERY values exclude `\s`.
// Two patterns can cross a newline, and the functions below report them:
// - SECRET_JSON_FIELD allows `\s*` (including newlines) around its colon and a
//   raw newline inside the quoted value;
// - PRIVATE_KEY (`(?s)`) runs from its BEGIN line to the END line, or to the end
//   of the text when no END line follows.
// Exact credential values (see SecretValues) cross a newline only when the
// value itself contains one.

/// Byte ranges of the masked JSON secret fields in `text`. A field can span
/// lines, so a slice cut at a newline may hold only part of one.
pub(crate) fn json_secret_field_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    if !text.contains('"') {
        return Vec::new();
    }
    SECRET_JSON_FIELD
        .find_iter(text)
        .map(|found| found.range())
        .collect()
}

/// Byte ranges of the masked private-key blocks in `text`. A block ends at its
/// END line or, without one, at the end of `text`.
pub(crate) fn private_key_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    if !text.contains("PRIVATE KEY-----") {
        return Vec::new();
    }
    PRIVATE_KEY
        .find_iter(text)
        .map(|found| found.range())
        .collect()
}

/// Whether `text` holds a private-key BEGIN or END line. Without one, `text`
/// alone cannot tell whether it starts inside a key block.
pub(crate) fn has_private_key_marker(text: &str) -> bool {
    text.contains("PRIVATE KEY-----")
}

/// Whether `line` has the shape of a PEM private-key body line: 40 to 76
/// base64 characters mixing upper case, lower case and digits. Lower-case hex
/// digests and ordinary words do not qualify; a mixed-case hex string can.
pub(crate) fn looks_like_private_key_body(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    (40..=76).contains(&line.len())
        && line
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        && line.bytes().any(|byte| byte.is_ascii_uppercase())
        && line.bytes().any(|byte| byte.is_ascii_lowercase())
        && line.bytes().any(|byte| byte.is_ascii_digit())
}

/// Whether a JSON secret value that opens before byte `before` is still open
/// at the end of `text`. Its closing quote, and so the match, may lie past the
/// end of `text`; masking `text` alone would then show the value.
pub(crate) fn json_secret_value_open_at_end(text: &str, before: usize) -> bool {
    if !text.contains('"') {
        return false;
    }
    SECRET_JSON_OPENING
        .find_iter(text)
        .take_while(|opening| opening.start() < before)
        .any(|opening| {
            // Mirror the value grammar `(?:\\.|[^"\\])+`: a backslash escapes
            // any character except a newline, and an unescaped quote closes it.
            let mut chars = text[opening.end()..].chars();
            loop {
                match chars.next() {
                    None => return true,
                    Some('"') => return false,
                    Some('\\') => match chars.next() {
                        None => return true,
                        Some('\n') => return false,
                        Some(_) => {}
                    },
                    Some(_) => {}
                }
            }
        })
}

/// Byte-level entry point for the log sink. Invalid UTF-8 is decoded with
/// replacement characters before masking so malformed bytes cannot bypass it.
pub fn aft_redact_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Cow::Owned(
            aft_redactor(&String::from_utf8_lossy(bytes))
                .into_owned()
                .into_bytes(),
        );
    };
    match aft_redactor(text) {
        Cow::Borrowed(_) => Cow::Borrowed(bytes),
        Cow::Owned(masked) => Cow::Owned(masked.into_bytes()),
    }
}

/// Cut `text` to at most `max_bytes` (on a char boundary) and note how much
/// was dropped as `…(+N bytes)`. Used for payloads that are useful to glimpse
/// in a debug line but can be arbitrarily large (LSP params, parser errors).
pub fn truncate_for_log(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!("{}…(+{} bytes)", &text[..cut], text.len() - cut))
}

/// Describe a request line that must not itself be logged: its byte length and
/// a short SHA-256 prefix, enough to correlate with a copy the caller kept.
pub fn unlogged_input_summary(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let short: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("bytes={} sha256={short}", input.len())
}

/// A `key=` value is treated as a secret when it is long, uses only
/// token-safe characters, and mixes digits with upper- and lower-case letters
/// (an API key such as `AIzaSy…`). Words, paths and single-case hex hashes do
/// not qualify.
fn looks_like_secret(value: &str) -> bool {
    value.len() >= KEY_VALUE_MIN_SECRET_LEN
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_-.~+/=".contains(ch))
        && value.chars().any(|ch| ch.is_ascii_digit())
        && value.chars().any(|ch| ch.is_ascii_uppercase())
        && value.chars().any(|ch| ch.is_ascii_lowercase())
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Apply one pattern, keeping the borrowed input when nothing changed. A match
/// can re-produce its own text (an already-masked `***@`, or a `key=` value
/// that does not look secret), so equality is checked, not just match count.
fn replace_in<'a, R: Replacer>(
    text: Cow<'a, str>,
    mut range: Option<&mut std::ops::Range<usize>>,
    pattern: &Regex,
    mut replacer: R,
) -> Cow<'a, str> {
    // Capture storage is allocated by replace_all even when it finds no match.
    // Keep the overwhelmingly common no-secret path allocation-free.
    if !pattern.is_match(&text) {
        return text;
    }
    let mut masked = String::new();
    let mut copied = 0;
    for caps in pattern.captures_iter(&text) {
        let found = caps.get(0).unwrap();
        masked.push_str(&text[copied..found.start()]);
        let from = masked.len();
        replacer.replace_append(&caps, &mut masked);
        if masked[from..] != *found.as_str() {
            if let Some(range) = range.as_deref_mut() {
                map_redacted_range(range, from, from + found.len(), masked.len() - from);
            }
        }
        copied = found.end();
    }
    masked.push_str(&text[copied..]);
    if masked == *text {
        text
    } else {
        Cow::Owned(masked)
    }
}

/// Keep a watch's raw match anchored when a replacement changes byte lengths.
/// A match touching a secret expands to cover the entire visible placeholder.
fn map_redacted_range(
    range: &mut std::ops::Range<usize>,
    from: usize,
    to: usize,
    replacement_len: usize,
) {
    let shift = |position: usize| {
        position
            .saturating_sub(to - from)
            .saturating_add(replacement_len)
    };
    if range.start >= to {
        range.start = shift(range.start);
        range.end = shift(range.end);
    } else if range.end > from {
        range.start = range.start.min(from);
        range.end = if range.end >= to {
            shift(range.end)
        } else {
            from + replacement_len
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_json_fields_and_private_keys_match_across_a_newline() {
        let line_anchored: [(&str, &Regex); 12] = [
            ("AUTHORIZATION_HEADER", &AUTHORIZATION_HEADER),
            ("API_KEY_HEADER", &API_KEY_HEADER),
            ("AWS_SECRET_FIELD", &AWS_SECRET_FIELD),
            ("SLACK_TOKEN", &SLACK_TOKEN),
            ("GITHUB_PAT", &GITHUB_PAT),
            ("GITHUB_TOKEN", &GITHUB_TOKEN),
            ("SK_API_KEY", &SK_API_KEY),
            ("JWT", &JWT),
            ("AWS_ACCESS_KEY_ID", &AWS_ACCESS_KEY_ID),
            ("URL_USERINFO", &URL_USERINFO),
            ("CREDENTIAL_QUERY", &CREDENTIAL_QUERY),
            ("KEY_QUERY", &KEY_QUERY),
        ];
        let samples = [
            "Authorization: Bearer opaque-auth-value".to_string(),
            "Proxy-Authorization:\tBasic dXNlcjpwYXNz==".to_string(),
            "x-api-key: opaque-api-key-value".to_string(),
            "aws_secret_access_key = abc/def+ghi".to_string(),
            "xoxb-12345-slack-token".to_string(),
            "github_pat_11ABCDEFG0123456789_abcdef".to_string(),
            format!("ghp_{}", "a".repeat(36)),
            "sk-abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            "eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl".to_string(),
            "AKIAABCDEFGHIJKLMNOP".to_string(),
            "https://user:password@example.com/path".to_string(),
            "https://e.com/p?access_token=opaque&key=AbCdEfGh12345678Zz".to_string(),
        ];
        for sample in &samples {
            for (cut, _) in sample.char_indices().skip(1) {
                let text = format!("{}\n{}", &sample[..cut], &sample[cut..]);
                for (name, pattern) in line_anchored {
                    for found in pattern.find_iter(&text) {
                        assert!(
                            !found.as_str().contains('\n'),
                            "{name} crossed a newline in {text:?}"
                        );
                    }
                }
            }
        }
        // The two exceptions, which watch previews widen their region for.
        assert!(SECRET_JSON_FIELD.is_match("{\"api_key\":\n  \"value\"}"));
        assert!(SECRET_JSON_FIELD.is_match("{\"api_key\":\"first\nsecond\"}"));
        assert!(PRIVATE_KEY.is_match("-----BEGIN PRIVATE KEY-----\nbody\n"));
    }

    #[test]
    fn json_secret_value_open_at_end_follows_the_value_grammar() {
        let opening = "{\"api_key\": \"";
        let open = |text: &str| json_secret_value_open_at_end(text, text.len());
        assert!(open(&format!("{opening}abc\ndef")));
        assert!(open(&format!("{opening}abc\\")));
        assert!(!open(&format!("{opening}abc\\\"def\" more")));
        // A backslash before a newline ends the value without a match.
        assert!(!open(&format!("{opening}abc\\\ndef")));
        // Openings at or after `before` are outside the caller's region.
        assert!(!json_secret_value_open_at_end(
            &format!("x {opening}abc"),
            1
        ));
    }

    #[test]
    fn ticket_digest_budget_refuses_before_hashing() {
        let digest = blake3::hash(b"0123456789abcdef0123456789abcdef")
            .to_hex()
            .to_string();
        TICKET_HASH_PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Box::new(|| panic!("over-budget input was hashed")))
        });
        assert!(
            redact_ticket_digest(&"a".repeat(TICKET_REDACTION_WINDOWS + 32), Some(&digest))
                .is_err()
        );
        TICKET_HASH_PROBE.with(|probe| *probe.borrow_mut() = None);
    }

    #[test]
    fn ticket_digest_hashes_only_complete_hex_windows() {
        let digest = blake3::hash(b"0123456789abcdef0123456789abcdef")
            .to_hex()
            .to_string();
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let probe_count = count.clone();
        TICKET_HASH_PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Box::new(move || probe_count.set(probe_count.get() + 1)))
        });
        let text = format!(
            "{} {} {} {}",
            "a".repeat(31),
            "g".repeat(1024),
            "a".repeat(32),
            "F".repeat(33)
        );
        assert_eq!(redact_ticket_digest(&text, Some(&digest)).unwrap(), text);
        TICKET_HASH_PROBE.with(|probe| *probe.borrow_mut() = None);
        assert_eq!(count.get(), 3);
    }

    #[test]
    fn completion_exact_env_values_are_case_sensitive_and_overlap_safe() {
        let environment = [
            ("DEPLOY_tOkEn", "abcdefgh"),
            ("MY_SECRET", "fghijklm"),
            ("api_KEY", "key-value-long"),
            ("PASSWORD", "password-value"),
            ("PASSWD", "passwd-value"),
            ("CREDENTIALS", "credential-value"),
            ("AUTHORIZATION", "auth-value"),
            ("tiny_TOKEN", "short"),
            ("NORMAL_VALUE", "public-value"),
        ];
        let secrets = SecretValues::from_environment(
            environment
                .iter()
                .map(|(key, value)| (std::ffi::OsStr::new(key), std::ffi::OsStr::new(value))),
        );
        assert_eq!(
            secrets.redact("abcdefghijklm/abcdefgh"),
            format!("{SECRET_PLACEHOLDER}/{SECRET_PLACEHOLDER}")
        );
        for (_, value) in &environment[..7] {
            assert_eq!(
                secrets.redact(&format!("before{value}after")),
                format!("before{SECRET_PLACEHOLDER}after")
            );
        }
        assert!(matches!(
            secrets.redact("ABCDEFGH short public-value"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn completion_non_secret_corpus_stays_borrowed() {
        for text in [
            "running 42 tests\ntest task::sk-settings ... ok\ntest result: ok. 42 passed; 0 failed\n",
            "commit 0f78c1bcb3ab4d501d6ebc491617b097416f9138\nAuthor: Example <user@example.com>\n    add task-settings\n",
            "added 42 packages, and audited 128 packages in 2s\n12 packages are looking for funding\nfound 0 vulnerabilities\n",
            "request 550e8400-e29b-41d4-a716-446655440000 fixture SGVsbG8gd29ybGQhYWJjZGVmZ2g=\n",
            "sk-settings sk-something sk-words docs say sk- is a prefix; https://example.com/path\n",
        ] {
            assert!(matches!(SecretValues::default().redact(text), Cow::Borrowed(_)), "mangled: {text}");
        }
    }

    #[test]
    fn completion_64k_noop_is_allocation_free() {
        let mut text = "running 42 tests\ntest task::sk-settings ... ok\n{\"status\":\"ok\"}\nhttps://example.com/path\n".repeat(1024);
        text.truncate(64 * 1024);
        let secrets = SecretValues::default();
        let _ = secrets.redact(&text); // Initialize regex automata outside the measurement.
        let start = std::time::Instant::now();
        let (out, allocations) = crate::test_allocations::count(|| secrets.redact(&text));
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(allocations, 0);
        eprintln!(
            "64 KiB completion redaction: {:?}, {allocations} allocations",
            start.elapsed()
        );
    }
    #[test]
    fn github_prefilter_only_scans_actual_token_prefixes() {
        for ordinary in [
            "high throughput",
            "right path",
            "bright light",
            "github repository",
        ] {
            assert!(
                !super::needs_github_token_scan(ordinary),
                "unnecessary scan: {ordinary}"
            );
        }
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let token = format!("{prefix}1234567890abcdefghij");
            assert!(super::needs_github_token_scan(&token));
            assert_eq!(super::aft_redactor(&token), super::SECRET_PLACEHOLDER);
        }
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        masked: Vec<MaskedCase>,
        untouched: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct MaskedCase {
        input: String,
        expected: String,
    }

    #[derive(serde::Deserialize)]
    struct CommandFixture {
        masked: Vec<MaskedCase>,
        untouched: Vec<String>,
    }

    /// Credentials typed inline in shell commands are masked, and none of the
    /// ordinary commands beside them change: the untouched list holds every
    /// command line from this repository's scripts, workflows and package.json
    /// scripts plus common cargo, git, npm, ssh and docker invocations.
    #[test]
    fn command_credentials_are_masked_without_touching_ordinary_commands() {
        let fixture: CommandFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/command_redaction_cases.json"
        ))
        .expect("command redaction fixture parses");
        assert_eq!(fixture.masked.len(), 101);
        assert!(fixture.untouched.len() > 800);
        for case in &fixture.masked {
            let masked = redact_command(&case.input);
            assert_eq!(masked, case.expected, "input: {}", case.input);
            assert_eq!(redact_command(&masked), masked, "not idempotent");
        }
        // A few corpus lines hold URL userinfo (`ssh://git@host`), which the
        // shared log patterns already mask in every log line. The command
        // shapes themselves must change nothing.
        let mangled: Vec<_> = fixture
            .untouched
            .iter()
            .filter(|command| redact_command(command) != aft_redactor(command))
            .map(|command| format!("{command} -> {}", redact_command(command)))
            .collect();
        assert!(mangled.is_empty(), "{}", mangled.join("\n"));
    }

    /// The curl, MySQL and sshpass rules as they were before commands were
    /// read as shell words: the span ran to the first `;`, `&`, `|` or
    /// newline, quoted or not.
    fn line_pattern_tool_passwords(command: &str) -> String {
        static PATTERNS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
            [
                compile(r"\bcurl\b[^;&|\n]*"),
                compile(concat!(
                    r"(^|\s)(-u[ \t]*|--user(?:=|[ \t]+))(",
                    shell_word!(),
                    ")"
                )),
                compile(concat!(
                    r"\b((?:mysql|mariadb|mysqladmin|mysqldump)\b[^;&|\n]*?\s-p)(",
                    shell_word!(),
                    ")"
                )),
                compile(concat!(r"\b(sshpass[ \t]+-p[ \t]*)(", shell_word!(), ")")),
            ]
        });
        let [segment, user_option, mysql, sshpass] = &*PATTERNS;
        let out = segment.replace_all(command, |segment: &Captures<'_>| {
            user_option
                .replace_all(
                    &segment[0],
                    |caps: &Captures<'_>| match mask_curl_password(&caps[3]) {
                        Some(masked) => format!("{}{}{masked}", &caps[1], &caps[2]),
                        None => caps[0].to_string(),
                    },
                )
                .into_owned()
        });
        let mut out = out.into_owned();
        for pattern in [mysql, sshpass] {
            out = pattern
                .replace_all(&out, |caps: &Captures<'_>| {
                    if command_value_is_reference(&caps[2]) {
                        caps[0].to_string()
                    } else {
                        format!("{}{SECRET_PLACEHOLDER}", &caps[1])
                    }
                })
                .into_owned();
        }
        out
    }

    /// Reading commands as shell words must not lose a password the earlier
    /// line patterns caught: on every fixture command, each placeholder the
    /// line patterns wrote is also written by the shell-word scan, at the
    /// same place.
    #[test]
    fn shell_word_scan_masks_everything_the_line_patterns_did() {
        let fixture: CommandFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/command_redaction_cases.json"
        ))
        .expect("command redaction fixture parses");
        let mut compared = 0;
        let mut lost = Vec::new();
        for command in fixture
            .masked
            .iter()
            .map(|case| &case.input)
            .chain(&fixture.untouched)
        {
            let legacy = line_pattern_tool_passwords(command);
            if legacy == *command {
                continue;
            }
            compared += 1;
            let scanned = mask_tool_passwords(command);
            // Text before each legacy placeholder must match the scan's
            // output up to a placeholder too.
            let legacy_prefixes: Vec<_> = legacy
                .match_indices(SECRET_PLACEHOLDER)
                .map(|(at, _)| &legacy[..at])
                .collect();
            let scanned_prefixes: Vec<_> = scanned
                .match_indices(SECRET_PLACEHOLDER)
                .map(|(at, _)| &scanned[..at])
                .collect();
            for prefix in legacy_prefixes {
                if !scanned_prefixes.iter().any(|scanned| {
                    scanned.trim_end_matches(['"', '\'']) == prefix.trim_end_matches(['"', '\''])
                }) {
                    lost.push(format!(
                        "{command:?}\n  line patterns: {legacy}\n  shell words:   {scanned}"
                    ));
                }
            }
        }
        assert!(lost.is_empty(), "{}", lost.join("\n"));
        eprintln!("{compared} fixture commands masked by the line patterns; none lost");
        assert!(compared > 40, "{compared}");
    }

    /// Whether `output` is `input` with some spans replaced by placeholders
    /// and every other byte unchanged and in order.
    fn only_placeholders_changed(input: &str, output: &str) -> bool {
        // Split `output` at its placeholders. Each placeholder's next
        // occurrence is searched for only after the previous one is passed,
        // so splitting a megabyte output stays linear.
        let placeholders = [SECRET_PLACEHOLDER, URL_CREDENTIAL_PLACEHOLDER];
        let find_from =
            |from: usize, placeholder: &str| output[from..].find(placeholder).map(|at| from + at);
        let mut next: Vec<Option<usize>> = placeholders
            .iter()
            .map(|placeholder| find_from(0, placeholder))
            .collect();
        let mut pieces = Vec::new();
        let mut copied = 0;
        loop {
            let nearest = next
                .iter()
                .enumerate()
                .filter_map(|(index, at)| at.map(|at| (at, index)))
                .min();
            let Some((at, index)) = nearest else {
                pieces.push(&output[copied..]);
                break;
            };
            pieces.push(&output[copied..at]);
            copied = at + placeholders[index].len();
            for (slot, placeholder) in next.iter_mut().zip(placeholders) {
                if slot.is_some_and(|found| found < copied) {
                    *slot = find_from(copied, placeholder);
                }
            }
        }
        if pieces.len() == 1 {
            return input == output;
        }
        let (first, last) = (pieces[0], pieces[pieces.len() - 1]);
        if first.len() + last.len() > input.len()
            || !input.starts_with(first)
            || !input.ends_with(last)
        {
            return false;
        }
        let limit = input.len() - last.len();
        let mut cursor = first.len();
        for piece in &pieces[1..pieces.len() - 1] {
            if cursor > limit {
                return false;
            }
            match input[cursor..limit].find(piece) {
                Some(at) => cursor += at + piece.len(),
                None => return false,
            }
        }
        cursor <= limit
    }

    /// The command redactor runs on every spawned command, so it must never
    /// panic, whatever the command holds. Twenty thousand seeded random
    /// commands mix quotes, backslashes, substitutions, operators, newlines
    /// and multibyte text (combining marks, emoji), with unterminated quotes
    /// and trailing backslashes. Each output must be `input` with only
    /// placeholder replacements.
    #[test]
    fn command_redaction_never_panics_and_changes_only_masked_bytes() {
        const PIECES: [&str; 44] = [
            "'",
            "\"",
            "\\",
            "$(",
            ")",
            "(",
            "`",
            "<",
            ">",
            "&",
            "|",
            ";",
            "\n",
            " ",
            "\t",
            "é",
            "中",
            "🙂",
            "e\u{301}",
            "\u{301}",
            "\u{200d}",
            "curl",
            "mysql",
            "sshpass",
            "-u",
            "-p",
            "--user=",
            "--password",
            "-ppw",
            "admin:pw",
            ":",
            "a",
            "1",
            "=",
            "TOKEN=",
            "$X",
            "2>&1",
            "<<<",
            "\\\n",
            "-f",
            "env",
            "sudo",
            "\"-u",
            "x'y",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let redactors: [(&str, fn(&str) -> Cow<'_, str>); 2] = [
            ("mask_tool_passwords", mask_tool_passwords),
            ("redact_command", redact_command),
        ];
        let mut failures = Vec::new();
        let mut masked = 0;
        for case in 0..20_000 {
            let mut input = String::new();
            for _ in 0..next() % 40 {
                input.push_str(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            if case % 7 == 0 {
                input.push('\\');
            }
            for (name, redact) in redactors {
                match std::panic::catch_unwind(|| redact(&input).into_owned()) {
                    Err(_) => failures.push(format!("{name} panicked on {input:?}")),
                    Ok(output) => {
                        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
                        masked += usize::from(output != input);
                        if !only_placeholders_changed(&input, &output) {
                            failures.push(format!(
                                "{name} changed other bytes: {input:?} -> {output:?}"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} failures, first: {}",
            failures.len(),
            failures
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        // The generator must reach the masking paths, not only ordinary text.
        assert!(masked > 1_000, "{masked} outputs masked");
    }

    /// One command of a megabyte or more, in shapes that would make a
    /// rescanning or per-program-word design quadratic, is scanned in steps
    /// linear in its length: each nesting level tokenizes each byte once and
    /// visits each token once, and quoted commands nest at most
    /// QUOTED_COMMAND_DEPTH levels below the top.
    #[test]
    fn tool_password_scan_work_is_linear_on_a_megabyte_command() {
        const MEGABYTE: usize = 1 << 20;
        let repeat = |unit: &str| unit.repeat(MEGABYTE / unit.len() + 1);
        let inputs = [
            repeat("curl "),
            repeat("curl -u a:b --next -u "),
            repeat("mysql -ppw sshpass -p x "),
            format!("bash -c \"{}", repeat("sh -c 'curl \\\"x ")),
            format!("curl {}", repeat("\"'")),
            format!("curl '{}", repeat("é中🙂 \"' ")),
            format!("curl {}", repeat("\\")),
            format!("curl {}", repeat("<<<")),
            format!("curl \"{}\"", repeat("'\\\"`$( curl -u a:b ) ")),
            format!("\"'\"'\"'{}", repeat("curl -u a:b ")),
        ];
        for input in &inputs {
            let bound = 3 * (QUOTED_COMMAND_DEPTH + 1) * input.len();
            SHELL_SCAN_STEPS.with(|steps| steps.set(0));
            SHELL_SCAN_BUDGET.with(|budget| budget.set(bound));
            let started = std::time::Instant::now();
            let masked = mask_tool_passwords(input);
            SHELL_SCAN_BUDGET.with(|budget| budget.set(usize::MAX));
            let steps = SHELL_SCAN_STEPS.with(std::cell::Cell::get);
            let elapsed = started.elapsed();
            assert!(
                steps <= bound,
                "{steps} steps for {} bytes (bound {bound}) starting {:?}",
                input.len(),
                input.chars().take(40).collect::<String>()
            );
            assert!(only_placeholders_changed(input, &masked));
            let started = std::time::Instant::now();
            let _ = redact_command(input);
            eprintln!(
                "{} bytes starting {:?}: {steps} scan steps, scan {elapsed:?}, full redaction {:?}",
                input.len(),
                input.chars().take(12).collect::<String>(),
                started.elapsed()
            );
        }
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../tests/fixtures/log_redaction_cases.json"))
            .expect("shared redaction fixture parses")
    }

    #[test]
    fn shared_fixture_masked_cases_match_the_cli_sanitizer_output() {
        let fixture = fixture();
        assert!(!fixture.masked.is_empty());
        for case in fixture.masked {
            assert_eq!(
                aft_redactor(&case.input),
                case.expected,
                "input: {}",
                case.input
            );
        }
    }

    #[test]
    fn shared_fixture_untouched_cases_are_borrowed_unchanged() {
        let fixture = fixture();
        assert!(!fixture.untouched.is_empty());
        for input in fixture.untouched {
            let out = aft_redactor(&input);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {input} -> {out}");
        }
    }

    #[test]
    fn masks_each_github_token_prefix() {
        let body = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let line = format!("token {prefix}{body} end");
            assert_eq!(
                aft_redactor(&line),
                "token <REDACTED_SECRET> end",
                "{prefix}"
            );
        }
        assert_eq!(
            aft_redactor("x github_pat_11AB_cd34 y"),
            "x <REDACTED_SECRET> y"
        );
    }

    #[test]
    fn masks_url_userinfo_for_any_scheme() {
        assert_eq!(
            aft_redactor("https://user:pass@host.example/path"),
            "https://***@host.example/path"
        );
        assert_eq!(
            aft_redactor("ssh://deploy@host.example:22/repo"),
            "ssh://***@host.example:22/repo"
        );
    }

    #[test]
    fn masks_credential_query_parameters() {
        assert_eq!(
            aft_redactor("u?token=abc&access_token=def&api_key=ghi&client_secret=jkl"),
            "u?token=<REDACTED_SECRET>&access_token=<REDACTED_SECRET>&api_key=<REDACTED_SECRET>&client_secret=<REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("u?key=AIzaSyA1b2C3d4E5f6G7h8I9j0 done"),
            "u?key=<REDACTED_SECRET> done"
        );
    }

    #[test]
    fn masks_header_jwt_sk_and_aws_shapes() {
        assert_eq!(
            aft_redactor("Authorization: Bearer abc.def-ghi"),
            "Authorization: Bearer <REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("jwt eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl end"),
            "jwt <REDACTED_SECRET> end"
        );
        assert_eq!(
            aft_redactor("key sk-live-abcdefgh123"),
            "key <REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("id AKIAABCDEFGHIJKLMNOP"),
            "id <REDACTED_SECRET>"
        );
    }

    #[test]
    fn redaction_is_idempotent() {
        for case in fixture().masked {
            let once = aft_redactor(&case.input).into_owned();
            assert!(matches!(aft_redactor(&once), Cow::Borrowed(_)), "{once}");
        }
    }

    #[test]
    fn ordinary_log_content_is_not_mangled() {
        for line in [
            "[aft] index_event kind=ready plane=search root=/Users/me/proj key=search:v3",
            "[aft] format: src/main.rs (rustfmt)",
            "sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "blake3 AF1349B9F5F9A1A6A0404DEA36DCC9499BCB25C9ADC112B7CC9A93CAE41F3262",
            "task_id=bgb_0123456789 session=ses_abcDEF0123456789",
            "https://example.com/search?key=ab12CD34&q=tokenizer",
            "url https://example.com/a?page_size=10&sort=asc",
            "ghost gh_pages ghp_tooShort",
        ] {
            let out = aft_redactor(line);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {line} -> {out}");
        }
    }

    #[test]
    fn truncate_for_log_bounds_bytes_and_reports_the_remainder() {
        assert_eq!(truncate_for_log("short", 10), "short");
        assert_eq!(truncate_for_log("abcdefghij", 4), "abcd…(+6 bytes)");
        // "é" is two bytes; the cut backs off to the char boundary.
        assert_eq!(truncate_for_log("aéb", 2), "a…(+3 bytes)");
    }

    #[test]
    fn unlogged_input_summary_carries_length_and_hash_only() {
        let input = "{\"command\":\"secret\"";
        let summary = unlogged_input_summary(input);
        let prefix = format!("bytes={} sha256=", input.len());
        assert!(summary.starts_with(&prefix), "{summary}");
        assert_eq!(summary.len(), prefix.len() + 12);
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn contextual_secret_formats_are_masked() {
        for (input, secret) in [
            ("x-api-key: private-key-value", "private-key-value"),
            (
                "Authorization: Token private-token-value",
                "private-token-value",
            ),
            (
                r#"{"password":"private-password-value"}"#,
                "private-password-value",
            ),
            (
                "token xoxb-12345-private-slack-value",
                "xoxb-12345-private-slack-value",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nprivate-pem-value\n-----END RSA PRIVATE KEY-----",
                "private-pem-value",
            ),
            (
                "AWS_SECRET_ACCESS_KEY=private-aws-secret-value",
                "private-aws-secret-value",
            ),
        ] {
            let masked = aft_redactor(input);
            assert!(!masked.contains(secret), "secret survived: {input}");
            assert!(masked.contains(SECRET_PLACEHOLDER));
            assert_eq!(aft_redactor(&masked), masked);
        }
        assert_eq!(
            aft_redactor("plain private-aws-secret-value"),
            "plain private-aws-secret-value"
        );
    }

    #[test]
    fn invalid_utf8_does_not_bypass_masking() {
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(b" ghp_1234567890abcdefghij");
        let masked = aft_redact_bytes(&bytes);
        assert!(!String::from_utf8_lossy(&masked).contains("ghp_"));
        assert!(std::str::from_utf8(&masked).is_ok());
    }

    #[test]
    fn non_utf8_bytes_are_safely_decoded() {
        let bytes = [0xff, 0xfe, b'g', b'h'];
        assert_eq!(aft_redact_bytes(&bytes).as_ref(), "��gh".as_bytes());
    }

    #[test]
    fn git_numbered_config_names_are_not_secret_names() {
        // Scrubbing GIT_CONFIG_KEY_0 alone leaves GIT_CONFIG_COUNT pointing at a
        // missing key, and git refuses to run.
        for key in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_KEY_12", "git_config_key_1"] {
            assert!(!is_secret_environment_key(key), "{key}");
        }
        for key in [
            "GITHUB_TOKEN",
            "NPM_AUTH",
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
        ] {
            assert!(is_secret_environment_key(key), "{key}");
        }
    }

    #[test]
    fn git_runs_after_the_secret_name_scrub() {
        let mut command = std::process::Command::new("git");
        command
            .args(["config", "--get", "core.hooksPath"])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null");
        for key in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"] {
            if is_secret_environment_key(key) {
                command.env_remove(key);
            }
        }
        let output = command.output().expect("git runs");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "/dev/null");
    }
}

//! Read-only CLI schema lookups, kept separate from bash output compression.
//!
//! Only live tasks retain their launch policy. Replayed tasks deliberately do
//! not probe: persisted metadata is not authority to reconstruct a sandbox.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::bash_rewrite::parser::{parse, ParsedCommand};
use crate::sandbox_spawn::SpawnPlan;

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const PROBE_OUTPUT_CAP: usize = 256 * 1024;
const BLOCK_CAP: usize = 2048;

#[derive(Default)]
struct Metrics {
    hinted: AtomicU64,
    no_candidate_table: AtomicU64,
    probe_timeout: AtomicU64,
    probe_error: AtomicU64,
    skipped_switch_off: AtomicU64,
    skipped_unsupported_target: AtomicU64,
}
static METRICS: Metrics = Metrics {
    hinted: AtomicU64::new(0),
    no_candidate_table: AtomicU64::new(0),
    probe_timeout: AtomicU64::new(0),
    probe_error: AtomicU64::new(0),
    skipped_switch_off: AtomicU64::new(0),
    skipped_unsupported_target: AtomicU64::new(0),
};

pub(crate) fn metrics() -> serde_json::Value {
    serde_json::json!({
        "hinted": METRICS.hinted.load(Ordering::Relaxed),
        "no_candidate_table": METRICS.no_candidate_table.load(Ordering::Relaxed),
        "probe_timeout": METRICS.probe_timeout.load(Ordering::Relaxed),
        "probe_error": METRICS.probe_error.load(Ordering::Relaxed),
        "skipped_switch_off": METRICS.skipped_switch_off.load(Ordering::Relaxed),
        "skipped_unsupported_target": METRICS.skipped_unsupported_target.load(Ordering::Relaxed),
    })
}

struct DbTarget {
    binary: PathBuf,
    database: PathBuf,
    connection: OsString,
    cwd: PathBuf,
    query: String,
}

enum SchemaMiss {
    Table(String),
    Column { name: String, tables: Vec<String> },
}

struct ProbeCommand {
    args: Vec<OsString>,
}

trait DbCliHint: Sync {
    fn detect(&self, cmd: &ParsedCommand, cwd: &Path) -> Option<DbTarget>;
    fn miss(&self, output: &str) -> Option<SchemaMiss>;
    fn candidates(&self, target: &DbTarget, miss: &mut SchemaMiss);
    fn probe(&self, target: &DbTarget, miss: &SchemaMiss) -> ProbeCommand;
    fn render(&self, target: &DbTarget, miss: &SchemaMiss, output: &str) -> Option<String>;
}

struct Sqlite;
static SQLITE: Sqlite = Sqlite;
static REGISTRY: &[&dyn DbCliHint] = &[&SQLITE];

/// A once cell serializes status polling and completion publication so they
/// cannot launch duplicate probes. Neither the command nor its output is changed.
pub(crate) struct HintJob {
    cli: Option<&'static dyn DbCliHint>,
    target: Option<DbTarget>,
    enabled: bool,
    pipeline: bool,
    // Release payload descriptors and request environment at terminal rendering,
    // not when the session's retained task history is eventually collected.
    launch: std::sync::Mutex<Option<(SpawnPlan, HashMap<String, String>)>>,
    result: OnceLock<Option<String>>,
}

impl HintJob {
    pub(crate) fn new(
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        plan: &SpawnPlan,
        enabled: bool,
    ) -> Self {
        let found = detect(command, cwd, env);
        let pipeline = shell_segments(command)
            .is_some_and(|segments| segments.iter().any(|(_, separator)| *separator == "|"));
        let (cli, mut target) = match found {
            Some((cli, target)) => (Some(cli), Some(target)),
            None => (None, None),
        };
        if let Some(target) = target.as_mut() {
            let environment = crate::sandbox_spawn::approved_environment_for_plan(plan, env);
            let path = environment
                .get(std::ffi::OsStr::new("PATH"))
                .map(|p| p.as_os_str())
                .unwrap_or(crate::effective_path::effective_path());
            if let Some(binary) = resolve_binary(&target.binary, &target.cwd, path) {
                target.binary = binary;
            } else {
                return Self {
                    cli: None,
                    target: None,
                    enabled,
                    pipeline,
                    launch: std::sync::Mutex::new(None),
                    result: OnceLock::new(),
                };
            }
        }
        let launch = target.as_ref().map(|_| (plan.clone(), env.clone()));
        Self {
            cli,
            target,
            enabled,
            pipeline,
            launch: std::sync::Mutex::new(launch),
            result: OnceLock::new(),
        }
    }

    pub(crate) fn finish(&self, output: &str, exit_code: Option<i64>) -> Option<&str> {
        self.finish_with_output(|| output, exit_code)
    }

    /// Acquire output only if this unfinished attempt needs diagnostic text.
    /// Successful non-pipelines still finish the job and release launch handles.
    pub(crate) fn finish_with_output<T: AsRef<str>>(
        &self,
        output: impl FnOnce() -> T,
        exit_code: Option<i64>,
    ) -> Option<&str> {
        self.result
            .get_or_init(|| {
                let launch = self.launch.lock().ok()?.take();
                // A successful query may itself print words resembling an error.
                // Pipelines are exempt: their last member can mask SQLite's exit.
                if exit_code == Some(0) && !self.pipeline {
                    return None;
                }
                let output = output();
                let output = output.as_ref();
                // Ordinary bash calls are not schema-hint attempts. Count skips only
                // when a supported CLI's missing-schema diagnostic was observed.
                let cli = self.cli.or_else(|| {
                    REGISTRY
                        .iter()
                        .copied()
                        .find(|cli| cli.miss(output).is_some())
                })?;
                let mut miss = cli.miss(output)?;
                if !self.enabled {
                    METRICS.skipped_switch_off.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                let Some(target) = self.target.as_ref().filter(|_| self.cli.is_some()) else {
                    METRICS
                        .skipped_unsupported_target
                        .fetch_add(1, Ordering::Relaxed);
                    return None;
                };
                cli.candidates(target, &mut miss);
                let probe = cli.probe(target, &miss);
                let (plan, env) = launch?;
                match run_probe(target, &probe, &plan, &env, PROBE_TIMEOUT) {
                    Ok(output) => {
                        let rendered = cli.render(target, &miss, &output);
                        if rendered.is_some() {
                            METRICS.hinted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            METRICS.probe_error.fetch_add(1, Ordering::Relaxed);
                        }
                        rendered
                    }
                    Err(ProbeFailure::Timeout) => {
                        METRICS.probe_timeout.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Err(ProbeFailure::Error) => {
                        METRICS.probe_error.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                }
            })
            .as_deref()
    }

    pub(crate) fn hint(&self) -> Option<&str> {
        self.result.get().and_then(|result| result.as_deref())
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.result.get().is_some()
    }
}

fn resolve_binary(binary: &Path, cwd: &Path, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: PathBuf| {
        std::fs::metadata(&p)
            .ok()
            .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .map(|_| p)
    };
    if binary.components().count() > 1 || binary.is_absolute() {
        return executable(if binary.is_absolute() {
            binary.to_path_buf()
        } else {
            cwd.join(binary)
        });
    }
    std::env::split_paths(path).find_map(|p| {
        executable(if p.is_absolute() {
            p.join(binary)
        } else {
            cwd.join(p).join(binary)
        })
    })
}

fn detect(
    command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<(&'static dyn DbCliHint, DbTarget)> {
    let segments = shell_segments(command)?;
    let mut cwd = cwd.to_path_buf();
    let mut vars: HashMap<String, String> = HashMap::new();
    let home = env
        .get("HOME")
        .cloned()
        .or_else(|| std::env::var("HOME").ok());
    let home_ref = home.as_deref();
    let mut found = None;
    let mut in_pipeline = false;
    for (raw_segment, separator) in segments {
        let segment = raw_segment.trim();
        if segment.is_empty() {
            if separator.is_empty() {
                continue;
            }
            return None;
        }
        // Leading `VAR=value` assignments establish the database path for a
        // later segment (`DB=...; sqlite3 "$DB" ...`). The value itself may
        // use `~` or earlier variables; command substitution stays unsupported.
        if let Some((name, raw_value)) = parse_hint_assignment(segment) {
            if raw_value.contains("$(") || raw_value.contains('`') {
                return None;
            }
            let expanded = expand_hint_segment(&raw_value, &vars, env, home_ref)?;
            vars.insert(name, strip_outer_quotes(&expanded));
            if separator == "|" {
                return None;
            }
            continue;
        }
        let expanded = expand_hint_segment(segment, &vars, env, home_ref)?;
        let parsed = parse(expanded.trim())?;
        if parsed.args.first().map(String::as_str) == Some("cd") {
            if in_pipeline || parsed.args.len() != 2 || (separator != "&&" && separator != ";") {
                return None;
            }
            cwd = cwd.join(&parsed.args[1]);
        } else if let Some(target) = REGISTRY
            .iter()
            .find_map(|cli| cli.detect(&parsed, &cwd).map(|t| (*cli, t)))
        {
            if found.is_some() {
                return None;
            }
            found = Some(target);
        } else if !in_pipeline {
            return None;
        }
        if (separator == "&&" || separator == ";") && parsed.args[0] != "cd" {
            return None;
        }
        if separator == "|" {
            in_pipeline = true;
        }
    }
    found
}

/// A single `NAME=value` (or `export NAME=value`) statement with no trailing
/// command. Segments with unquoted whitespace belong to a command, not a
/// pure assignment, so `VAR=value sqlite3 ...` prefix forms stay unsupported.
fn parse_hint_assignment(segment: &str) -> Option<(String, String)> {
    let rest = match segment.strip_prefix("export") {
        Some(after) if after.starts_with([' ', '\t']) => after.trim_start(),
        Some(_) => return None,
        None => segment,
    };
    // `export` alone (without a name) is not an assignment.
    if rest.len() != segment.len() && rest.trim().is_empty() {
        return None;
    }
    let eq = rest.find('=')?;
    let (lhs, rhs) = rest.split_at(eq);
    let name = lhs.trim();
    if !is_hint_var_name(name) {
        return None;
    }
    let rhs = &rhs[1..];
    if has_unquoted_hint_space(rhs) {
        return None;
    }
    Some((name.to_string(), rhs.trim().to_string()))
}

fn is_hint_var_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 256 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_unquoted_hint_space(value: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some(ch) {
            quote = None;
            continue;
        }
        if quote.is_some() {
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            return true;
        }
    }
    false
}

fn strip_outer_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Expand leading `~` and `$NAME`/`${NAME}` for hint detection only, honoring
/// shell quoting: no expansion inside single quotes, no `~` inside double
/// quotes. Command substitution (`$(...)`), backticks outside single quotes,
/// `~otheruser`, and special parameters (`$?`, `$$`, ...) decline the command
/// so the probe never guesses a target. Unknown variables are left verbatim
/// for the literal parser to decline.
fn expand_hint_segment(
    segment: &str,
    vars: &HashMap<String, String>,
    env: &HashMap<String, String>,
    home: Option<&str>,
) -> Option<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = String::with_capacity(segment.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && quote != Some('\'') {
            out.push(ch);
            if let Some(next) = chars.get(i + 1) {
                out.push(*next);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if quote == Some(ch) {
            quote = None;
            out.push(ch);
            i += 1;
            continue;
        }
        if quote.is_some() {
            if quote == Some('"') {
                if ch == '`' {
                    return None;
                }
                if ch == '$' {
                    match expand_hint_dollar(&chars, i, vars, env, &mut out)? {
                        Some(next) => {
                            i = next;
                            continue;
                        }
                        None => return None,
                    }
                } else {
                    out.push(ch);
                    i += 1;
                }
                continue;
            }
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '`' {
            return None;
        }
        if ch == '~' && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '=' | ':')) {
            let next = chars.get(i + 1);
            if next.is_none()
                || matches!(next, Some(' ' | '\t' | ';' | '&' | '|' | '"' | '\'' | '/'))
            {
                out.push_str(home?);
                i += 1;
                continue;
            }
            return None;
        }
        if ch == '$' {
            match expand_hint_dollar(&chars, i, vars, env, &mut out)? {
                Some(next) => {
                    i = next;
                    continue;
                }
                None => return None,
            }
        }
        out.push(ch);
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    Some(out)
}

/// Expand one `$NAME` or `${NAME}` at `chars[i] == '$'`, appending to `out`.
/// Returns the index after the expansion, or `None` for unsupported forms.
/// Unknown names are left verbatim so the literal parser still declines them.
fn expand_hint_dollar(
    chars: &[char],
    i: usize,
    vars: &HashMap<String, String>,
    env: &HashMap<String, String>,
    out: &mut String,
) -> Option<Option<usize>> {
    let next = chars.get(i + 1)?;
    if *next == '(' {
        return Some(None);
    }
    if *next == '{' {
        let rest = &chars[i + 2..];
        let end = rest.iter().position(|&c| c == '}')?;
        let name: String = rest[..end].iter().collect();
        if !is_hint_var_name(&name) {
            return Some(None);
        }
        if let Some(value) = vars.get(&name).or_else(|| env.get(&name)) {
            out.push_str(value);
        } else {
            out.push_str(&format!("${{{name}}}"));
        }
        return Some(Some(i + 2 + end + 1));
    }
    if next.is_ascii_alphabetic() || *next == '_' {
        let mut j = i + 2;
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        let name: String = chars[i + 1..j].iter().collect();
        if let Some(value) = vars.get(&name).or_else(|| env.get(&name)) {
            out.push_str(value);
        } else {
            out.push('$');
            out.push_str(&name);
        }
        return Some(Some(j));
    }
    Some(None)
}

/// Split unquoted `&&`, `;` and pipes; the existing parser then proves every
/// word is literal. Other shell control flow cannot establish a stable target.
fn shell_segments(command: &str) -> Option<Vec<(&str, &str)>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some(ch) {
            quote = None;
            continue;
        }
        if quote.is_some() {
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if matches!(ch, '\n' | '(' | ')' | '`') {
            return None;
        }
        let separator = match ch {
            '&' if chars.peek().is_some_and(|(_, c)| *c == '&') => {
                chars.next();
                "&&"
            }
            '|' if !chars.peek().is_some_and(|(_, c)| matches!(c, '|' | '&')) => "|",
            ';' => ";",
            '&' | '|' => return None,
            _ => continue,
        };
        segments.push((&command[start..index], separator));
        start = index + separator.len();
    }
    if quote.is_some() || escaped {
        return None;
    }
    segments.push((&command[start..], ""));
    Some(segments)
}

impl DbCliHint for Sqlite {
    fn detect(&self, cmd: &ParsedCommand, cwd: &Path) -> Option<DbTarget> {
        let binary = PathBuf::from(cmd.args.first()?);
        if binary.file_name()? != "sqlite3" {
            return None;
        }
        let mut index = 1;
        while cmd.args.get(index)?.starts_with('-') {
            match cmd.args[index].as_str() {
                "-separator" | "-nullvalue" | "-newline" => index += 2,
                // Startup commands can reopen a different database or attach
                // connection-local schemas; the positional path is not proof
                // of the database which actually produced the error.
                "-cmd" | "-init" => return None,
                "-readonly" | "-batch" | "-bail" | "-header" | "-noheader" | "-csv" | "-column"
                | "-line" | "-list" | "-json" | "-table" | "-quote" | "-tabs" | "-box" => {
                    index += 1
                }
                _ => return None,
            }
        }
        let database = &cmd.args[index];
        let (database_path, connection) = literal_database(database, cwd)?;
        let query = cmd.args[index + 1..].join(" ");
        if query.trim_start().starts_with('.') {
            return None;
        }
        Some(DbTarget {
            binary,
            database: database_path,
            connection,
            cwd: cwd.to_path_buf(),
            query,
        })
    }

    fn miss(&self, output: &str) -> Option<SchemaMiss> {
        for line in output.lines() {
            for (needle, column) in [("no such table: ", false), ("no such column: ", true)] {
                if let Some((_, name)) = line.split_once(needle) {
                    let name = name.trim().trim_end_matches(" (1)");
                    if name.is_empty() || name.len() > 256 {
                        continue;
                    }
                    return Some(if column {
                        SchemaMiss::Column {
                            name: name.to_string(),
                            tables: Vec::new(),
                        }
                    } else {
                        SchemaMiss::Table(name.to_string())
                    });
                }
            }
        }
        None
    }

    fn candidates(&self, target: &DbTarget, miss: &mut SchemaMiss) {
        if let SchemaMiss::Column { tables, .. } = miss {
            *tables = query_tables(&target.query);
        }
    }

    fn probe(&self, target: &DbTarget, miss: &SchemaMiss) -> ProbeCommand {
        let names = "SELECT name FROM sqlite_schema WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'";
        let sql = match miss {
            SchemaMiss::Table(_) => format!("SELECT name FROM ({names}) ORDER BY name"),
            SchemaMiss::Column { tables, .. } => {
                let predicate = if tables.is_empty() {
                    format!("(SELECT count(*) FROM ({names})) = 1")
                } else {
                    format!(
                        "s.name COLLATE NOCASE IN ({})",
                        tables
                            .iter()
                            .map(|t| sql_string(t))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                // The table-valued PRAGMA is the read-only table_info lookup.
                // JSON preserves identifiers containing whitespace or delimiters.
                format!("SELECT s.name, (SELECT json_group_array(json_object('name',p.name,'type',p.type)) FROM pragma_table_info(s.name) p) AS columns FROM ({names}) s WHERE {predicate} UNION ALL SELECT name, NULL AS columns FROM ({names}) WHERE NOT EXISTS (SELECT 1 FROM ({names}) s WHERE {predicate})")
            }
        };
        // An explicit empty init file prevents a user's .sqliterc from running
        // arbitrary startup SQL. No flags from the failing command are copied.
        ProbeCommand {
            args: vec![
                "-readonly".into(),
                "-batch".into(),
                "-init".into(),
                "/dev/null".into(),
                "-json".into(),
                target.connection.clone(),
                sql.into(),
            ],
        }
    }

    fn render(&self, target: &DbTarget, miss: &SchemaMiss, output: &str) -> Option<String> {
        let rows: Vec<serde_json::Value> = serde_json::from_str(if output.trim().is_empty() {
            "[]"
        } else {
            output
        })
        .ok()?;
        let db = safe_text(&target.database.file_name()?.to_string_lossy());
        let names: Vec<String> = rows
            .iter()
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect();
        match miss {
            SchemaMiss::Table(name) => Some(table_list(
                format!(
                    "[aft: no table {} in {db}; {} tables]",
                    sql_string(&safe_text(name)),
                    names.len()
                ),
                names,
                name,
            )),
            SchemaMiss::Column { name, .. } => {
                let mut tables = BTreeMap::new();
                for row in &rows {
                    if let Some(columns) = row["columns"].as_str() {
                        let columns: Vec<serde_json::Value> = serde_json::from_str(columns).ok()?;
                        let rendered = columns
                            .iter()
                            .map(|c| {
                                format!(
                                    "{} {}",
                                    safe_text(c["name"].as_str().unwrap_or("")),
                                    safe_text(c["type"].as_str().unwrap_or(""))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        tables.insert(safe_text(row["name"].as_str()?), rendered);
                    }
                }
                if tables.is_empty() {
                    METRICS.no_candidate_table.fetch_add(1, Ordering::Relaxed);
                    return Some(table_list(
                        format!(
                            "[aft: no column {} in {db}; {} tables]",
                            sql_string(&safe_text(name)),
                            names.len()
                        ),
                        names,
                        name,
                    ));
                }
                let table_names = tables.keys().cloned().collect::<Vec<_>>().join(", ");
                let header = if table_names.len() < 512 {
                    format!(
                        "[aft: no column {} in table {} ({db})]",
                        sql_string(&safe_text(name)),
                        table_names
                    )
                } else {
                    format!(
                        "[aft: no column {} in {db}; {} candidate tables]",
                        sql_string(&safe_text(name)),
                        tables.len()
                    )
                };
                let mut block = header;
                let total = tables.len();
                let mut shown = 0;
                for (table, columns) in tables {
                    let line = format!("\n{table}: {columns}");
                    if block.len() + line.len() + 64 > BLOCK_CAP {
                        break;
                    }
                    block.push_str(&line);
                    shown += 1;
                }
                if shown < total {
                    block.push_str(&format!(
                        "\n… {} table schemas omitted (size cap)",
                        total - shown
                    ));
                }
                Some(block)
            }
        }
    }
}

fn literal_database(argument: &str, cwd: &Path) -> Option<(PathBuf, OsString)> {
    if argument.is_empty() || argument == ":memory:" || argument.contains(['$', '*', '[', ']']) {
        return None;
    }
    if let Some(uri) = argument.strip_prefix("file:") {
        let base = url::Url::from_directory_path(cwd).ok()?;
        let uri = if uri.starts_with('/') {
            url::Url::parse(argument).ok()?
        } else {
            base.join(uri).ok()?
        };
        if uri.fragment().is_some() {
            return None;
        }
        let mut readonly = false;
        for (key, value) in uri.query_pairs() {
            match (key.as_ref(), value.as_ref()) {
                ("mode", "ro") => readonly = true,
                ("immutable", "0" | "1") | ("cache", "private") => {}
                _ => return None,
            }
        }
        if !readonly {
            return None;
        }
        let path = uri.to_file_path().ok()?;
        if path.file_name()? == ":memory:" {
            return None;
        }
        return Some((path, uri.as_str().into()));
    }
    if argument.contains('?') {
        return None;
    }
    let path = cwd.join(argument);
    Some((path.clone(), path.into_os_string()))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn table_list(header: String, mut names: Vec<String>, missing: &str) -> String {
    if header.len() + names.iter().map(|n| n.len() + 2).sum::<usize>() > BLOCK_CAP {
        names.sort_by_cached_key(|name| {
            (
                edit_distance(&name.to_lowercase(), &missing.to_lowercase()),
                name.clone(),
            )
        });
    } else {
        names.sort();
    }
    let mut block = format!("{header}\n");
    let total = names.len();
    let mut shown = 0;
    for name in names {
        let name = safe_text(&name);
        if block.len() + name.len() + 64 > BLOCK_CAP {
            break;
        }
        if shown > 0 {
            block.push_str(", ");
        }
        block.push_str(&name);
        shown += 1;
    }
    if shown < total {
        block.push_str(&format!(", … {} tables omitted", total - shown));
    }
    block
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let previous = row[j + 1];
            row[j + 1] = (row[j] + 1)
                .min(previous + 1)
                .min(diagonal + usize::from(a != *b));
            diagonal = previous;
        }
    }
    row[right.len()]
}

fn query_tables(query: &str) -> Vec<String> {
    // SQL strings and comments are not clauses. Preserve quoted identifiers,
    // but decline subqueries, CTE aliases and attached-schema qualifiers.
    let pattern = r#"(?is)'(?:''|[^'])*'|--[^\n]*|/\*.*?\*/|\b(?:FROM|JOIN)\s+(\"(?:\"\"|[^\"])+\"|`(?:``|[^`])+`|\[[^\]]+\]|[a-z_][a-z_0-9]*)(?:\s*\.)?"#;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(pattern).unwrap());
    let mut tables = Vec::new();
    for cap in re.captures_iter(query) {
        let Some(name) = cap.get(1) else {
            continue;
        };
        if cap[0].trim_end().ends_with('.') {
            continue;
        }
        let name = name.as_str();
        let name = if name.starts_with('"') {
            name[1..name.len() - 1].replace("\"\"", "\"")
        } else if name.starts_with('`') {
            name[1..name.len() - 1].replace("``", "`")
        } else if name.starts_with('[') {
            name[1..name.len() - 1].to_string()
        } else {
            name.to_string()
        };
        if !tables.contains(&name) {
            tables.push(name);
        }
    }
    tables
}

#[derive(Debug, PartialEq)]
enum ProbeFailure {
    Timeout,
    Error,
}

fn run_probe(
    target: &DbTarget,
    probe: &ProbeCommand,
    plan: &SpawnPlan,
    env: &HashMap<String, String>,
    timeout: Duration,
) -> Result<String, ProbeFailure> {
    let deadline = Instant::now() + timeout;
    let mut command = crate::sandbox_spawn::probe_command_for_plan(
        plan,
        target.binary.as_os_str(),
        &probe.args,
        &target.cwd,
        env,
    )
    .map_err(|_| ProbeFailure::Error)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProbeFailure::Error)?;
    let mut stdout = child.stdout.take().ok_or(ProbeFailure::Error)?;
    let mut stderr = child.stderr.take().ok_or(ProbeFailure::Error)?;
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProbeFailure::Error);
        }
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = 'probe: loop {
        if Instant::now() >= deadline {
            break Err(ProbeFailure::Timeout);
        }
        let mut eof = true;
        for (reader, bytes) in [
            (&mut stdout as &mut dyn Read, &mut out),
            (&mut stderr as &mut dyn Read, &mut err),
        ] {
            let mut chunk = [0u8; 4096];
            match reader.read(&mut chunk) {
                Ok(0) => {}
                Ok(n) => {
                    bytes.extend_from_slice(&chunk[..n]);
                    eof = false;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => eof = false,
                Err(_) => break 'probe Err(ProbeFailure::Error),
            }
        }
        if out.len() + err.len() > PROBE_OUTPUT_CAP {
            break Err(ProbeFailure::Error);
        }
        match child.try_wait() {
            Ok(Some(status)) if eof => {
                // Judge the probe by its exit status and JSON stdout only. The
                // sandbox launcher reports unenforceable rules on stderr (for
                // example under Landlock), which is not a probe failure.
                break if status.success() {
                    String::from_utf8(out).map_err(|_| ProbeFailure::Error)
                } else {
                    Err(ProbeFailure::Error)
                };
            }
            Err(_) => break Err(ProbeFailure::Error),
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    if result.is_err() {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod a02_lifecycle_tests {
    use super::*;

    #[test]
    fn a02_success_still_finishes_and_releases_launch_payload() {
        let job = HintJob {
            cli: None,
            target: None,
            enabled: true,
            pipeline: false,
            launch: std::sync::Mutex::new(Some((
                SpawnPlan::Unsandboxed,
                HashMap::from([("KEY".into(), "value".into())]),
            ))),
            result: OnceLock::new(),
        };
        assert_eq!(
            job.finish_with_output(
                || -> &str { panic!("successful non-pipeline must not read output") },
                Some(0)
            ),
            None
        );
        assert!(job.result.get().is_some());
        assert!(job.launch.lock().unwrap().is_none());
        assert_eq!(
            job.finish_with_output(
                || -> &str { panic!("finished job must not read output") },
                Some(1)
            ),
            None
        );
    }
}

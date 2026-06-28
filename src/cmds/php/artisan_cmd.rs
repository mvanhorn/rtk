//! Laravel `php artisan` output compression.
//!
//! Only read-only/status-style artisan subcommands are filtered. Everything else
//! is executed as a raw passthrough so interactive, destructive, or custom app
//! commands keep native behavior.

use crate::tracking;
use crate::utils::{exit_code_from_output, resolved_command, strip_ansi, truncate};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::process::{ExitStatus, Stdio};

const MAX_ROUTE_ROWS: usize = 80;
const MAX_MIGRATION_ROWS: usize = 40;
const MAX_GENERIC_ROWS: usize = 40;
const MAX_KV_LINES: usize = 80;

pub fn run(args: &[String], verbose: u8, skip_env: bool) -> Result<()> {
    if !should_filter(args, verbose) {
        return run_passthrough(args, verbose, skip_env);
    }

    let timer = tracking::TimedExecution::start();
    let mut cmd = artisan_command(args, skip_env);

    if verbose > 0 {
        eprintln!("Running: php {}", artisan_args(args).join(" "));
    }

    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => exit_spawn_error(err),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let raw = format!("{}{}", stdout, stderr);
    let exit_code = exit_code_from_output(&output, "artisan");

    if !output.status.success() {
        if !stdout.is_empty() {
            print!("{}", stdout);
        }
        if !stderr.is_empty() {
            eprint!("{}", stderr);
        }

        timer.track(
            &format!("php {}", artisan_args(args).join(" ")),
            &format!("rtk artisan {}", args.join(" ")),
            &raw,
            &raw,
        );

        std::process::exit(exit_code);
    }

    if !stderr.is_empty() {
        eprint!("{}", stderr);
    }

    let subcommand = args.first().map(String::as_str).unwrap_or("");
    let filtered = filter_artisan_output(subcommand, &stdout);
    println!("{}", filtered);

    let tracked_output = if stderr.is_empty() {
        filtered.clone()
    } else {
        format!("{}{}", filtered, stderr)
    };

    timer.track(
        &format!("php {}", artisan_args(args).join(" ")),
        &format!("rtk artisan {}", args.join(" ")),
        &raw,
        &tracked_output,
    );

    Ok(())
}

fn run_passthrough(args: &[String], verbose: u8, skip_env: bool) -> Result<()> {
    let timer = tracking::TimedExecution::start();
    let mut cmd = artisan_command(args, skip_env);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose > 0 {
        eprintln!("Running: php {}", artisan_args(args).join(" "));
    }

    let status = match cmd.status() {
        Ok(status) => status,
        Err(err) => exit_spawn_error(err),
    };

    let original = format!("php {}", artisan_args(args).join(" "));
    timer.track_passthrough(
        &original,
        &format!("rtk artisan {} (passthrough)", args.join(" ")),
    );

    if !status.success() {
        std::process::exit(exit_code_from_status(status, "artisan"));
    }

    Ok(())
}

fn artisan_command(args: &[String], skip_env: bool) -> std::process::Command {
    let mut cmd = resolved_command("php");
    cmd.args(artisan_args(args));
    if skip_env {
        cmd.env("SKIP_ENV_VALIDATION", "1");
    }
    cmd
}

fn artisan_args(args: &[String]) -> Vec<String> {
    std::iter::once("artisan".to_string())
        .chain(args.iter().cloned())
        .collect()
}

fn exit_spawn_error(err: std::io::Error) -> ! {
    eprintln!("[rtk: failed to run php artisan: {}]", err);
    let code = if err.kind() == ErrorKind::NotFound {
        127
    } else {
        1
    };
    std::process::exit(code);
}

fn exit_code_from_status(status: ExitStatus, label: &str) -> i32 {
    match status.code() {
        Some(code) => code,
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    eprintln!("[rtk] {}: process terminated by signal {}", label, sig);
                    return 128 + sig;
                }
            }
            eprintln!("[rtk] {}: process terminated by signal", label);
            1
        }
    }
}

fn should_filter(args: &[String], verbose: u8) -> bool {
    if verbose > 0 || args.iter().any(|arg| is_passthrough_arg(arg)) {
        return false;
    }

    matches!(
        args.first().map(String::as_str),
        Some("route:list" | "migrate:status" | "about" | "db:show")
    )
}

fn is_passthrough_arg(arg: &str) -> bool {
    matches!(arg, "--json" | "--help" | "-h" | "-V" | "--version")
        || arg.starts_with("--format=json")
}

fn filter_artisan_output(subcommand: &str, output: &str) -> String {
    let clean = strip_ansi(output);
    if clean.trim().is_empty() {
        return format!("ok php artisan {}", subcommand);
    }

    match subcommand {
        "route:list" => filter_route_list(&clean),
        "migrate:status" => filter_migrate_status(&clean),
        "about" => filter_key_value_output("about", &clean),
        "db:show" => filter_db_show(&clean),
        _ => clean,
    }
}

fn filter_route_list(output: &str) -> String {
    if let Some(table) = parse_ascii_table(output) {
        let method_idx = find_column(&table.headers, &["method", "verb"]);
        let uri_idx = find_column(&table.headers, &["uri", "path"]);
        let name_idx = find_column(&table.headers, &["name"]);
        let action_idx = find_column(&table.headers, &["action"]);

        let mut routes = Vec::new();
        let mut method_counts: BTreeMap<String, usize> = BTreeMap::new();

        for row in &table.rows {
            let method = cell(row, method_idx).unwrap_or_default();
            let uri = cell(row, uri_idx).unwrap_or_default();
            if method.is_empty() && uri.is_empty() {
                continue;
            }

            *method_counts.entry(method.to_string()).or_default() += 1;

            let mut line = format!("{} {}", method, uri);
            if let Some(name) = cell(row, name_idx) {
                if !name.is_empty() {
                    line.push_str(&format!(" name={}", name));
                }
            }
            if let Some(action) = cell(row, action_idx) {
                if !action.is_empty() {
                    line.push_str(&format!(" action={}", truncate(action, 80)));
                }
            }
            routes.push(line);
        }

        return build_route_summary(routes, method_counts);
    }

    let mut routes = Vec::new();
    let mut method_counts: BTreeMap<String, usize> = BTreeMap::new();

    for line in output.lines() {
        if let Some((method, uri, details)) = parse_route_line(line) {
            *method_counts.entry(method.to_string()).or_default() += 1;
            let mut compact = format!("{} {}", method, uri);
            if !details.is_empty() {
                compact.push_str(&format!(" {}", truncate(&details, 100)));
            }
            routes.push(compact);
        }
    }

    if routes.is_empty() {
        output.to_string()
    } else {
        build_route_summary(routes, method_counts)
    }
}

fn build_route_summary(routes: Vec<String>, method_counts: BTreeMap<String, usize>) -> String {
    let mut result = Vec::new();
    let counts = method_counts
        .iter()
        .map(|(method, count)| format!("{}={}", method, count))
        .collect::<Vec<_>>()
        .join(" ");

    if counts.is_empty() {
        result.push(format!("routes: {}", routes.len()));
    } else {
        result.push(format!("routes: {} ({})", routes.len(), counts));
    }

    result.extend(routes.iter().take(MAX_ROUTE_ROWS).cloned());
    if routes.len() > MAX_ROUTE_ROWS {
        result.push(format!(
            "... +{} more routes",
            routes.len() - MAX_ROUTE_ROWS
        ));
    }

    result.join("\n")
}

fn parse_route_line(line: &str) -> Option<(String, String, String)> {
    let trimmed = line.trim();
    let mut parts = trimmed.split_whitespace();
    let method = parts.next()?;

    if !looks_like_http_method(method) {
        return None;
    }

    let uri = parts.next()?.to_string();
    let details = parts.collect::<Vec<_>>().join(" ");
    let details = details
        .split_whitespace()
        .filter(|part| !part.chars().all(|ch| ch == '.'))
        .collect::<Vec<_>>()
        .join(" ");

    Some((method.to_string(), uri, details))
}

fn looks_like_http_method(value: &str) -> bool {
    value.split('|').all(|part| {
        matches!(
            part,
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
        )
    })
}

fn filter_migrate_status(output: &str) -> String {
    let mut ran = Vec::new();
    let mut pending = Vec::new();

    if let Some(table) = parse_ascii_table(output) {
        let ran_idx = find_column(&table.headers, &["ran?", "ran", "status"]);
        let migration_idx = find_column(&table.headers, &["migration", "name"]);
        let batch_idx = find_column(&table.headers, &["batch"]);

        for row in &table.rows {
            let migration = cell(row, migration_idx).unwrap_or_default();
            if migration.is_empty() {
                continue;
            }

            let ran_value = cell(row, ran_idx).unwrap_or_default().to_ascii_lowercase();
            let batch = cell(row, batch_idx).unwrap_or_default();
            if ran_value.contains("yes") || ran_value == "ran" || !batch.is_empty() {
                ran.push(migration.to_string());
            } else {
                pending.push(migration.to_string());
            }
        }
    } else {
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("Migration name") {
                continue;
            }
            if trimmed.contains(" Pending")
                || trimmed.ends_with("Pending")
                || trimmed.contains("No")
            {
                pending.push(trim_migration_status_line(trimmed));
            } else if trimmed.contains(" Ran")
                || trimmed.ends_with("Ran")
                || trimmed.contains("Yes")
            {
                ran.push(trim_migration_status_line(trimmed));
            }
        }
    }

    if ran.is_empty() && pending.is_empty() {
        return output.to_string();
    }

    let mut result = Vec::new();
    result.push(format!(
        "migrations: {} ran, {} pending",
        ran.len(),
        pending.len()
    ));

    if !pending.is_empty() {
        result.push("pending:".to_string());
        result.extend(
            pending
                .iter()
                .take(MAX_MIGRATION_ROWS)
                .map(|m| format!("  {}", m)),
        );
        if pending.len() > MAX_MIGRATION_ROWS {
            result.push(format!(
                "  ... +{} more pending",
                pending.len() - MAX_MIGRATION_ROWS
            ));
        }
    }

    if !ran.is_empty() {
        result.push("latest ran:".to_string());
        let start = ran.len().saturating_sub(5);
        result.extend(ran[start..].iter().map(|m| format!("  {}", m)));
    }

    result.join("\n")
}

fn trim_migration_status_line(line: &str) -> String {
    line.split_whitespace()
        .filter(|part| {
            !part.chars().all(|ch| ch == '.')
                && !matches!(*part, "Ran" | "Pending" | "Yes" | "No")
                && !part.starts_with('[')
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn filter_db_show(output: &str) -> String {
    let fields = collect_key_value_rows(output);
    let table = parse_ascii_table(output);

    if fields.is_empty() {
        return table
            .as_ref()
            .map(|table| format_compact_table("db:show", table, MAX_GENERIC_ROWS))
            .unwrap_or_else(|| output.to_string());
    }

    let mut result = Vec::new();
    let field_total = fields.len();
    result.push(format!("db:show: {} fields", field_total));
    result.extend(fields.into_iter().take(MAX_KV_LINES));
    if field_total > MAX_KV_LINES {
        result.push(format!("... +{} more fields", field_total - MAX_KV_LINES));
    }

    if let Some(table) = table {
        result.push(String::new());
        result.push(format_compact_table("tables", &table, MAX_GENERIC_ROWS));
    }

    result.join("\n")
}

fn filter_key_value_output(label: &str, output: &str) -> String {
    let rows = collect_key_value_rows(output);

    if rows.is_empty() {
        if let Some(table) = parse_ascii_table(output) {
            format_compact_table(label, &table, MAX_GENERIC_ROWS)
        } else {
            output.to_string()
        }
    } else {
        let total = rows.len();
        let mut result = Vec::new();
        result.push(format!("{}: {} fields", label, total));
        result.extend(rows.into_iter().take(MAX_KV_LINES));
        if total > MAX_KV_LINES {
            result.push(format!("... +{} more fields", total - MAX_KV_LINES));
        }
        result.join("\n")
    }
}

fn collect_key_value_rows(output: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current_section = String::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_ascii_border(trimmed) {
            continue;
        }

        if let Some((key, value)) = split_dotted_key_value(trimmed) {
            let key = normalize_key(&key);
            if current_section.is_empty() {
                rows.push(format!("{}={}", key, value));
            } else {
                rows.push(format!("{}.{}={}", current_section, key, value));
            }
        } else if !trimmed.contains('|') {
            current_section = normalize_key(trimmed);
        }
    }

    rows
}

fn split_dotted_key_value(line: &str) -> Option<(String, String)> {
    let dots = line.find("...")?;
    let (key, rest) = line.split_at(dots);
    let value = rest.trim_start_matches('.').trim();
    if key.trim().is_empty() || value.is_empty() {
        return None;
    }
    Some((key.trim().to_string(), value.to_string()))
}

fn normalize_key(key: &str) -> String {
    key.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[derive(Debug, PartialEq)]
struct ParsedTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

fn parse_ascii_table(output: &str) -> Option<ParsedTable> {
    let mut rows = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_ascii_border(trimmed) {
            continue;
        }

        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells = split_table_cells(trimmed);
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
    }

    if rows.len() < 2 {
        return None;
    }

    let headers = rows.remove(0);
    Some(ParsedTable { headers, rows })
}

fn split_table_cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_matches('|');
    let chars = inner.char_indices().collect::<Vec<_>>();
    let mut cells = Vec::new();
    let mut start = 0;

    for (idx, (byte_idx, ch)) in chars.iter().enumerate() {
        if *ch != '|' {
            continue;
        }

        let prev = idx
            .checked_sub(1)
            .and_then(|i| chars.get(i))
            .map(|(_, c)| c.is_whitespace())
            .unwrap_or(true);
        let next = chars
            .get(idx + 1)
            .map(|(_, c)| c.is_whitespace())
            .unwrap_or(true);

        if prev && next {
            cells.push(inner[start..*byte_idx].trim().to_string());
            start = *byte_idx + ch.len_utf8();
        }
    }

    cells.push(inner[start..].trim().to_string());
    cells
}

fn is_ascii_border(line: &str) -> bool {
    line.len() >= 3
        && line
            .chars()
            .all(|ch| matches!(ch, '+' | '-' | '=' | ' ' | ':'))
        && (line.contains('-') || line.contains('='))
}

fn find_column(headers: &[String], names: &[&str]) -> Option<usize> {
    headers.iter().position(|header| {
        let normalized = header.trim().to_ascii_lowercase();
        names.iter().any(|name| normalized == *name)
    })
}

fn cell(row: &[String], idx: Option<usize>) -> Option<&str> {
    idx.and_then(|i| row.get(i)).map(|value| value.trim())
}

fn format_compact_table(label: &str, table: &ParsedTable, max_rows: usize) -> String {
    let mut result = Vec::new();
    result.push(format!("{}: {} rows", label, table.rows.len()));
    result.push(table.headers.join("\t"));
    result.extend(table.rows.iter().take(max_rows).map(|row| row.join("\t")));
    if table.rows.len() > max_rows {
        result.push(format!("... +{} more rows", table.rows.len() - max_rows));
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_artisan_args_preserve_flags_and_values() {
        let args = vec![
            "route:list".to_string(),
            "--path=api/v1".to_string(),
            "--columns=method,uri,name".to_string(),
            "quoted value".to_string(),
        ];

        assert_eq!(
            artisan_args(&args),
            vec![
                "artisan",
                "route:list",
                "--path=api/v1",
                "--columns=method,uri,name",
                "quoted value"
            ]
        );
    }

    #[test]
    fn test_should_filter_only_known_safe_subcommands() {
        assert!(should_filter(&["route:list".into()], 0));
        assert!(should_filter(&["migrate:status".into()], 0));
        assert!(should_filter(&["about".into()], 0));
        assert!(should_filter(&["db:show".into()], 0));

        assert!(!should_filter(&["migrate".into()], 0));
        assert!(!should_filter(&["tinker".into()], 0));
        assert!(!should_filter(&["app:custom".into()], 0));
        assert!(!should_filter(&["route:list".into(), "--json".into()], 0));
        assert!(!should_filter(&["route:list".into()], 1));
    }

    #[test]
    fn test_filter_route_list_table() {
        let output = r#"
+--------+----------+----------------+-------------+--------------------------------+
| Domain | Method   | URI            | Name        | Action                         |
+--------+----------+----------------+-------------+--------------------------------+
|        | GET|HEAD | /              | home        | App\Http\Controllers\Home@index |
|        | POST     | login          | login       | App\Http\Controllers\Auth@login |
+--------+----------+----------------+-------------+--------------------------------+
"#;

        let result = filter_artisan_output("route:list", output);

        assert!(result.contains("routes: 2"));
        assert!(result.contains("GET|HEAD / name=home"));
        assert!(result.contains("POST login name=login"));
        assert!(!result.contains("+--------"));
    }

    #[test]
    fn test_filter_route_list_text() {
        let output = r#"
  GET|HEAD        / ................................................ home
  POST            login ............................................ login
"#;

        let result = filter_artisan_output("route:list", output);

        assert!(result.contains("routes: 2"));
        assert!(result.contains("GET|HEAD / home"));
        assert!(result.contains("POST login login"));
        assert!(!result.contains("................................"));
    }

    #[test]
    fn test_filter_migrate_status_table() {
        let output = r#"
+------+------------------------------------------------+-------+
| Ran? | Migration                                      | Batch |
+------+------------------------------------------------+-------+
| Yes  | 2014_10_12_000000_create_users_table          | 1     |
| Yes  | 2014_10_12_100000_create_password_resets_table| 1     |
| No   | 2026_06_28_120000_add_artisan_metrics        |       |
+------+------------------------------------------------+-------+
"#;

        let result = filter_artisan_output("migrate:status", output);

        assert!(result.contains("migrations: 2 ran, 1 pending"));
        assert!(result.contains("pending:"));
        assert!(result.contains("2026_06_28_120000_add_artisan_metrics"));
        assert!(result.contains("latest ran:"));
        assert!(!result.contains("+------+"));
    }

    #[test]
    fn test_filter_about_key_values() {
        let output = r#"
  Environment
  Application Name ................................ Laravel
  Laravel Version ................................. 11.9.0

  Cache
  Config .......................................... CACHED
"#;

        let result = filter_artisan_output("about", output);

        assert!(result.contains("about: 3 fields"));
        assert!(result.contains("environment.application_name=Laravel"));
        assert!(result.contains("environment.laravel_version=11.9.0"));
        assert!(result.contains("cache.config=CACHED"));
    }

    #[test]
    fn test_filter_db_show_table() {
        let output = r#"
+----------+--------+-------+
| Table    | Size   | Rows  |
+----------+--------+-------+
| users    | 16 KiB | 12    |
| sessions | 32 KiB | 2048  |
+----------+--------+-------+
"#;

        let result = filter_artisan_output("db:show", output);

        assert!(result.contains("db:show: 2 rows"));
        assert!(result.contains("Table\tSize\tRows"));
        assert!(result.contains("users\t16 KiB\t12"));
        assert!(!result.contains("+----------"));
    }

    #[test]
    fn test_filter_db_show_mixed_fields_and_table() {
        let output = r#"
  Connection ................................ mysql
  Database .................................. app_test

+----------+--------+-------+
| Table    | Size   | Rows  |
+----------+--------+-------+
| users    | 16 KiB | 12    |
+----------+--------+-------+
"#;

        let result = filter_artisan_output("db:show", output);

        assert!(result.contains("db:show: 2 fields"));
        assert!(result.contains("connection=mysql"));
        assert!(result.contains("database=app_test"));
        assert!(result.contains("tables: 1 rows"));
        assert!(result.contains("users\t16 KiB\t12"));
    }
}

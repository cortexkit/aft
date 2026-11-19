fn main() -> std::io::Result<()> {
    use std::io::Write;
    let schema = if std::env::args_os().nth(1).as_deref() == Some("--declaration".as_ref()) {
        aft::config_schema::settings_declaration()
    } else {
        aft::config_resolve::schema::editor_schema()
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &schema)?;
    writeln!(out)
}

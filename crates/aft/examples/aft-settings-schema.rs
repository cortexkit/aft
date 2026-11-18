fn main() -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &aft::config_schema::editor_schema())?;
    writeln!(out)
}

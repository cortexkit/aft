//! Load and parse through the production loader after signing this executable
//! with macOS hardened runtime and no executable-memory entitlements.
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let storage = PathBuf::from(args.next().ok_or("missing isolated grammar storage root")?);
    let file = PathBuf::from(args.next().ok_or("missing Luau source file")?);
    if args.next().is_some() || !storage.is_absolute() || !file.is_absolute() {
        return Err("expected absolute storage root and Luau source paths".into());
    }
    let config = aft::config::Config {
        storage_dir: Some(storage),
        ..Default::default()
    };
    // The caller hand-places the published artifact in this isolated root.
    // Only the stored-load API is called; a missing artifact is a hard refusal.
    let registry = aft::grammar::configure(&config);
    registry.stored().map_err(|error| error.message)?;
    let (tree, language) = aft::parser::FileParser::new()
        .with_dynamic_languages(true)
        .parse_cloned(&file)
        .map_err(|error| error.to_error_json().to_string())?;
    if language != aft::parser::LangId::Luau
        || tree.root_node().kind() != "chunk"
        || tree.root_node().has_error()
    {
        return Err("the production Pulley grammar did not parse the Luau fixture".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "hash": registry.identity(),
            "engine_target": aft::grammar::engine_target().map_err(|error| error.message)?,
            "tree": tree.root_node().to_sexp(),
            "test_hooks": cfg!(feature = "test-hooks"),
        })
    );
    Ok(())
}

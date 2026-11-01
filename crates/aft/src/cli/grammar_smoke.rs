use std::ffi::OsString;
use std::path::PathBuf;

/// The production signed-card probe never calls `request`: an absent grammar
/// fails the probe instead of opening a network connection.
pub fn run(args: Vec<OsString>) -> Result<(), String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--storage-dir")) {
        return Err("usage: aft grammar-smoke --storage-dir <absolute-directory>".into());
    }
    let storage = PathBuf::from(args.next().ok_or("missing storage directory")?);
    if !storage.is_absolute() {
        return Err("grammar smoke storage directory must be absolute".into());
    }
    #[cfg(feature = "test-grammar-seams")]
    let download = args.next().is_some_and(|s| s == "--download");
    if args.next().is_some() {
        return Err("unexpected grammar smoke argument".into());
    }
    let mut config = aft::config::Config {
        storage_dir: Some(storage),
        cache_dir_opt_out: true,
        ..Default::default()
    };
    if let Some(warning) = aft::root_cache::prepare_cache_root(&mut config) {
        return Err(warning.message);
    }
    let registry = aft::grammar::configure(&config);
    #[cfg(feature = "test-grammar-seams")]
    if download {
        let _ = registry.request();
        if !registry.wait_until_settled(std::time::Duration::from_secs(40)) {
            return Err("grammar smoke download timed out".into());
        }
    }
    let language = registry
        .stored()
        .map_err(|e| serde_json::to_string(&e).unwrap_or_default())?;
    let mut parser = tree_sitter::Parser::new();
    aft::grammar::prepare_parser(&mut parser).map_err(|e| e.message)?;
    parser.set_language(&language).map_err(|e| e.to_string())?;
    let tree = parser
        .parse("local function f() return 1 end", None)
        .ok_or("grammar smoke parse returned no tree")?;
    if tree.root_node().has_error() || tree.root_node().kind() != "chunk" {
        return Err("grammar smoke parse failed".into());
    }
    let mut result = serde_json::json!({
        "language": "luau", "hash": registry.identity(), "compile_ms": registry.compile_ms(),
        "tree": tree.root_node().to_sexp(), "test_seams": cfg!(feature = "test-grammar-seams"),
    });
    #[cfg(feature = "test-grammar-seams")]
    {
        result["compile_count"] = registry.compile_count().into();
    }
    #[cfg(not(feature = "test-grammar-seams"))]
    let _ = &mut result;
    println!("{result}");
    Ok(())
}

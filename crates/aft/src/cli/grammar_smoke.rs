use std::ffi::OsString;
use std::path::PathBuf;

/// The production signed-card probe never calls `request`: an absent grammar
/// fails the probe instead of opening a network connection.
pub fn run(args: Vec<OsString>) -> Result<(), String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--storage-dir")) {
        return Err(
            "usage: aft grammar-smoke --storage-dir <absolute-directory> [--file <luau-file>]"
                .into(),
        );
    }
    let storage = PathBuf::from(args.next().ok_or("missing storage directory")?);
    if !storage.is_absolute() {
        return Err("grammar smoke storage directory must be absolute".into());
    }
    let mut file = None;
    #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
    let (mut download, mut parallel) = (false, false);
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--file") if file.is_none() => {
                file = Some(PathBuf::from(
                    args.next().ok_or("missing grammar smoke file")?,
                ));
            }
            #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
            Some("--download") => download = true,
            #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
            Some("--parallel") => parallel = true,
            _ => return Err("unexpected grammar smoke argument".into()),
        }
    }
    #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
    if parallel && file.is_none() {
        return Err("parallel grammar smoke requires --file".into());
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
    #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
    if download {
        let _ = registry.request();
        if !registry.wait_until_settled(std::time::Duration::from_secs(40)) {
            return Err("grammar smoke download timed out".into());
        }
    }
    let tree = if let Some(file) = file {
        #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
        if parallel {
            use rayon::prelude::*;
            let trees = rayon::ThreadPoolBuilder::new()
                .num_threads(8)
                .build()
                .map_err(|e| e.to_string())?
                .install(|| {
                    (0..32)
                        .into_par_iter()
                        .map(|_| parse_file(&file))
                        .collect::<Result<Vec<_>, _>>()
                })?;
            let first = trees[0].root_node().to_sexp();
            if trees.iter().any(|tree| tree.root_node().to_sexp() != first) {
                return Err("parallel grammar smoke trees differ".into());
            }
        }
        parse_file(&file)?
    } else {
        let language = registry
            .stored()
            .map_err(|e| serde_json::to_string(&e).unwrap_or_default())?;
        let mut parser = tree_sitter::Parser::new();
        aft::grammar::prepare_parser(&mut parser).map_err(|e| e.message)?;
        parser.set_language(&language).map_err(|e| e.to_string())?;
        parser
            .parse("local function f() return 1 end", None)
            .ok_or("grammar smoke parse returned no tree")?
    };
    if tree.root_node().has_error() || tree.root_node().kind() != "chunk" {
        return Err("grammar smoke parse failed".into());
    }
    let mut result = serde_json::json!({
        "language": "luau", "engine_target": "pulley64", "hash": registry.identity(), "compile_ms": registry.compile_ms(),
        "tree": tree.root_node().to_sexp(), "test_seams": cfg!(feature = "test-hooks"),
    });
    #[cfg(all(feature = "test-hooks", feature = "wasm-grammars"))]
    {
        result["compile_count"] = registry.compile_count().into();
    }
    #[cfg(not(all(feature = "test-hooks", feature = "wasm-grammars")))]
    let _ = &mut result;
    println!("{result}");
    Ok(())
}

fn parse_file(file: &std::path::Path) -> Result<tree_sitter::Tree, String> {
    let (tree, language) = aft::parser::FileParser::new()
        .with_dynamic_languages(true)
        .parse_cloned(file)
        .map_err(|error| error.to_error_json().to_string())?;
    if language != aft::parser::LangId::Luau {
        return Err("grammar smoke requires a Luau file".into());
    }
    Ok(tree)
}

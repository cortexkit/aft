use super::{
    complete_rust_trait_members, extract_symbols_from_tree, AftError, Direct, LangId, Symbol, Tree,
};

pub(super) fn extract(source: &str, tree: &Tree, lang: LangId) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = extract_symbols_from_tree(source, tree, lang)?;
    if lang == LangId::Rust {
        complete_rust_trait_members(
            &mut symbols,
            crate::callgraph::rust_trait_members(source, tree.root_node(), Direct),
        );
    }
    Ok(symbols)
}

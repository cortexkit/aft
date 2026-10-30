pub(crate) fn extract_item_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let declaration = if let Some(restricted) = trimmed.strip_prefix("pub(") {
        restricted.split_once(')')?.1.trim_start()
    } else {
        trimmed.strip_prefix("pub ").unwrap_or(trimmed).trim_start()
    };

    // Strip function qualifiers separately so `const LIMIT` remains a const
    // item, while `const unsafe extern "C" fn run` is named `run`, not `unsafe`.
    let mut function = declaration;
    while let Some(rest) = ["async ", "const ", "unsafe ", "extern \"C\" "]
        .iter()
        .find_map(|prefix| function.strip_prefix(prefix))
    {
        function = rest.trim_start();
    }
    let rest = function.strip_prefix("fn ").or_else(|| {
        ["const ", "static ", "struct ", "enum "]
            .iter()
            .find_map(|prefix| declaration.strip_prefix(prefix))
    })?;
    let name = rest
        .trim_start()
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect::<String>();
    (!name.is_empty()).then_some(name)
}

#[test]
fn visibility_and_qualifiers_preserve_item_names() {
    for visibility in [
        "",
        "pub ",
        "pub(crate) ",
        "pub(super) ",
        "pub(self) ",
        "pub(in crate::x) ",
    ] {
        for (declaration, expected) in [
            ("fn plain() {}", "plain"),
            ("async fn asynchronous() {}", "asynchronous"),
            ("const fn constant() {}", "constant"),
            ("unsafe fn unchecked() {}", "unchecked"),
            ("extern \"C\" fn foreign() {}", "foreign"),
            ("async unsafe fn combined() {}", "combined"),
            ("const unsafe extern \"C\" fn qualified() {}", "qualified"),
            ("const LIMIT: usize = 1;", "LIMIT"),
            ("static VALUES: &[usize] = &[];", "VALUES"),
            ("struct Record {}", "Record"),
            ("enum Choice {}", "Choice"),
        ] {
            let line = format!("    {visibility}{declaration}");
            assert_eq!(
                extract_item_name(&line).as_deref(),
                Some(expected),
                "{line}"
            );
        }
    }
}

#[test]
fn non_items_have_no_item_name() {
    for line in [
        "",
        "// pub(super) fn commented() {}",
        "let item = 1;",
        "impl Record {",
        "pub(in crate::x fn unclosed() {}",
        "pub(super) fn () {}",
    ] {
        assert_eq!(extract_item_name(line), None, "{line}");
    }
}

pub use aft_base::file_kind::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Read;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Component, Path};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;

#[cfg(test)]
mod tests {

    use super::*;
    #[test]
    fn extension_only_source_classification_matches_parser() {
        use crate::lang_id::{LangId, TEST_EXTENSIONS};
        for name in TEST_EXTENSIONS
            .iter()
            .map(|ext| format!("file.{ext}"))
            .chain(["Jenkinsfile", "file", ".hidden"].map(str::to_owned))
        {
            let expected = matches!(crate::parser::detect_language(std::path::Path::new(&name)),
                Some(lang) if !matches!(lang, LangId::Markdown | LangId::Html | LangId::Yaml | LangId::Toml));
            assert_eq!(super::is_source_file_name(&name), expected, "{name}");
        }
    }
}

#[cfg(test)]
mod generated;

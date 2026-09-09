mod pattern_compile {
    #![allow(dead_code)]
    include!("../../../../crates/aft/src/pattern_compile.rs");
}

mod query_shape {
    #![allow(dead_code)]
    include!("../../../../crates/aft/src/query_shape.rs");
}

use std::io::{self, BufRead};

fn main() {
    for line in io::stdin().lock().lines() {
        let line = line.expect("read query");
        let query: String = serde_json::from_str(&line).expect("JSON string query");
        let kind = query_shape::classify(&query).kind;
        println!(
            "{}",
            match kind {
                query_shape::QueryKind::Identifier => "identifier",
                query_shape::QueryKind::Mixed => "mixed",
                query_shape::QueryKind::ErrorCode => "error_code",
                query_shape::QueryKind::Path => "path",
                query_shape::QueryKind::Regex => "regex",
                query_shape::QueryKind::NaturalLanguage => "natural_language",
            }
        );
    }
}

use std::collections::HashSet;

use crate::commands::semantic_search::extensions::{SearchExtensions, Token, TokenVariant};

/// The variants lane shares one budget across every token in a query.
pub const MAX_QUERY_VARIANTS: usize = 6;

/// Deterministic implementation of the variants hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct OrderedVariantPipeline;

impl SearchExtensions for OrderedVariantPipeline {
    fn variants(&self, token: Token<'_>) -> Vec<TokenVariant> {
        generate_variants(token)
    }
}

/// Generates variants in token order, retaining the first emission globally.
pub fn generate_variants(input: Token<'_>) -> Vec<TokenVariant> {
    let mut variants = Vec::with_capacity(MAX_QUERY_VARIANTS);
    let mut seen = HashSet::new();

    for (token_offset, token) in input.text.split_whitespace().enumerate() {
        for candidate in variants_for_token(token) {
            if candidate != token && seen.insert(candidate.clone()) {
                variants.push(TokenVariant {
                    token_index: input.index + token_offset,
                    text: candidate,
                });
                if variants.len() == MAX_QUERY_VARIANTS {
                    return variants;
                }
            }
        }
    }

    variants
}

/// Returns only admitted variants, preserving their original generation order.
pub fn contributing_variants(
    generated: &[TokenVariant],
    admitted_variants: &[String],
) -> Vec<String> {
    let admitted = admitted_variants
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    generated
        .iter()
        .filter(|variant| admitted.contains(variant.text.as_str()))
        .map(|variant| variant.text.clone())
        .collect()
}

/// Renders the variants footer only when at least one variant admitted a result.
pub fn variants_footer(generated: &[TokenVariant], admitted_variants: &[String]) -> Option<String> {
    let contributing = contributing_variants(generated, admitted_variants);
    (!contributing.is_empty()).then(|| format!("variants applied: {}", contributing.join(", ")))
}

fn variants_for_token(token: &str) -> Vec<String> {
    let words = parse_words(token);
    if words.is_empty() {
        return Vec::new();
    }

    let leading_hash = token.starts_with('#');
    let mut variants = Vec::with_capacity(7);
    variants.push(convert_case(&words, leading_hash, CaseForm::Snake));
    variants.push(convert_case(&words, leading_hash, CaseForm::Kebab));
    variants.push(convert_case(&words, leading_hash, CaseForm::Camel));
    variants.push(convert_case(&words, leading_hash, CaseForm::Pascal));
    variants.push(convert_case(&words, leading_hash, CaseForm::ScreamingSnake));

    let last = words.last().expect("non-empty words have a last word");
    let toggled = toggle_number(last.text);
    variants.push(format!(
        "{}{}{}",
        &token[..last.start],
        toggled,
        &token[last.end..]
    ));

    if let Some(remainder) = hash_prefix_remainder(token) {
        variants.push(remainder.to_string());
    }

    variants
}

#[derive(Debug, Clone, Copy)]
struct Word<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn parse_words(token: &str) -> Vec<Word<'_>> {
    let body_start = usize::from(token.starts_with('#'));
    let body = &token[body_start..];
    let characters = body.char_indices().collect::<Vec<_>>();
    let mut words = Vec::new();
    let mut word_start = None;

    for (position, &(index, character)) in characters.iter().enumerate() {
        if is_declared_separator(character) {
            if let Some(start) = word_start.take() {
                push_word(&mut words, token, body_start + start, body_start + index);
            }
            continue;
        }

        if let Some(start) = word_start {
            let previous = characters[position - 1].1;
            let next = characters.get(position + 1).map(|(_, next)| *next);
            let lower_to_upper = previous.is_ascii_lowercase() && character.is_ascii_uppercase();
            let acronym_to_word = previous.is_ascii_uppercase()
                && character.is_ascii_uppercase()
                && next.is_some_and(|next| next.is_ascii_lowercase());
            if lower_to_upper || acronym_to_word {
                push_word(&mut words, token, body_start + start, body_start + index);
                word_start = Some(index);
            }
        } else {
            word_start = Some(index);
        }
    }

    if let Some(start) = word_start {
        push_word(&mut words, token, body_start + start, token.len());
    }

    words
}

fn push_word<'a>(words: &mut Vec<Word<'a>>, token: &'a str, start: usize, end: usize) {
    if start < end {
        words.push(Word {
            text: &token[start..end],
            start,
            end,
        });
    }
}

fn is_declared_separator(character: char) -> bool {
    matches!(character, '_' | '-' | '.' | '/')
}

#[derive(Debug, Clone, Copy)]
enum CaseForm {
    Snake,
    Kebab,
    Camel,
    Pascal,
    ScreamingSnake,
}

fn convert_case(words: &[Word<'_>], leading_hash: bool, form: CaseForm) -> String {
    let mut converted = String::new();
    if leading_hash {
        converted.push('#');
    }

    let separator = match form {
        CaseForm::Snake | CaseForm::ScreamingSnake => Some('_'),
        CaseForm::Kebab => Some('-'),
        CaseForm::Camel | CaseForm::Pascal => None,
    };

    for (index, word) in words.iter().enumerate() {
        if index > 0 {
            if let Some(separator) = separator {
                converted.push(separator);
            }
        }
        match form {
            CaseForm::Snake | CaseForm::Kebab => {
                converted.push_str(&word.text.to_ascii_lowercase());
            }
            CaseForm::Camel if index == 0 => {
                converted.push_str(&word.text.to_ascii_lowercase());
            }
            CaseForm::Camel | CaseForm::Pascal => {
                converted.push_str(&capitalize_ascii(word.text));
            }
            CaseForm::ScreamingSnake => {
                converted.push_str(&word.text.to_ascii_uppercase());
            }
        }
    }

    converted
}

fn capitalize_ascii(word: &str) -> String {
    let lowercase = word.to_ascii_lowercase();
    let mut characters = lowercase.chars();
    let Some(first) = characters.next() else {
        return lowercase;
    };
    let mut capitalized = String::with_capacity(lowercase.len());
    capitalized.extend(first.to_uppercase());
    capitalized.extend(characters);
    capitalized
}

const SINGULAR_EXCEPTIONS: [(&str, &str); 12] = [
    ("status", "statuses"),
    ("bus", "buses"),
    ("class", "classes"),
    ("process", "processes"),
    ("address", "addresses"),
    ("analysis", "analyses"),
    ("basis", "bases"),
    ("axis", "axes"),
    ("alias", "aliases"),
    ("canvas", "canvases"),
    ("focus", "focuses"),
    ("lens", "lenses"),
];

fn toggle_number(word: &str) -> String {
    let lowercase = word.to_ascii_lowercase();
    let toggled = if let Some((_, plural)) = SINGULAR_EXCEPTIONS
        .iter()
        .find(|(singular, _)| *singular == lowercase)
    {
        (*plural).to_string()
    } else if let Some((singular, _)) = SINGULAR_EXCEPTIONS
        .iter()
        .find(|(_, plural)| *plural == lowercase)
    {
        (*singular).to_string()
    } else if lowercase.ends_with('s') {
        singularize(&lowercase)
    } else {
        pluralize(&lowercase)
    };

    apply_case_pattern(word, &toggled)
}

fn singularize(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if let Some(stem) = word.strip_suffix("es") {
        if ends_with_sibilant(stem) {
            return stem.to_string();
        }
    }
    word.strip_suffix('s').unwrap_or(word).to_string()
}

fn pluralize(word: &str) -> String {
    if let Some(stem) = word.strip_suffix('y') {
        if stem
            .chars()
            .last()
            .is_some_and(|character| !is_ascii_vowel(character))
        {
            return format!("{stem}ies");
        }
    }
    if ends_with_sibilant(word) {
        format!("{word}es")
    } else {
        format!("{word}s")
    }
}

fn ends_with_sibilant(word: &str) -> bool {
    word.ends_with('s')
        || word.ends_with('x')
        || word.ends_with('z')
        || word.ends_with("ch")
        || word.ends_with("sh")
}

fn is_ascii_vowel(character: char) -> bool {
    matches!(character.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u')
}

fn apply_case_pattern(original: &str, replacement: &str) -> String {
    if original
        .chars()
        .all(|character| !character.is_ascii_lowercase())
    {
        replacement.to_ascii_uppercase()
    } else if original
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
        && original
            .chars()
            .skip(1)
            .all(|character| !character.is_ascii_uppercase())
    {
        capitalize_ascii(replacement)
    } else {
        replacement.to_string()
    }
}

fn hash_prefix_remainder(token: &str) -> Option<&str> {
    let body = token.strip_prefix('#').unwrap_or(token);
    let hex_length = body
        .bytes()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if hex_length < 6 {
        return None;
    }
    let separator = body.as_bytes().get(hex_length)?;
    if !matches!(separator, b'_' | b'-') {
        return None;
    }
    let remainder = &body[hex_length + 1..];
    (!remainder.is_empty()).then_some(remainder)
}

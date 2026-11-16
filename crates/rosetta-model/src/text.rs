//! Small text rules shared by parsers, the store and search.
use serde_json::Value;

/// Maximum length, in characters, of any string stored in `extra`.
pub const EXTRA_MAX_CHARS: usize = 200;
/// Maximum number of elements kept in an array stored in `extra`.
pub const EXTRA_MAX_ITEMS: usize = 100;

/// Name search tokens: split on every character that is not alphanumeric (this covers
/// whitespace `_ / : . , -`), then split camelCase (`getUser` -> get user) and acronyms followed
/// by a word (`HTTPClient` -> http client), lowercase, keep tokens of at least two characters.
/// Tokens are purely alphanumeric, so they can be put into a `to_tsquery` text unescaped.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            let boundary = ((prev.is_lowercase() || prev.is_ascii_digit()) && cur.is_uppercase())
                || (prev.is_uppercase() && cur.is_uppercase() && next_lower);
            if boundary {
                push_token(&mut tokens, &chars[start..i]);
                start = i;
            }
        }
        push_token(&mut tokens, &chars[start..]);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, chars: &[char]) {
    if chars.len() >= 2 {
        tokens.push(chars.iter().collect::<String>().to_lowercase());
    }
}

/// `text` cut to [`EXTRA_MAX_CHARS`] characters (never splits a character).
pub fn cut(text: &str) -> String {
    cut_to(text, EXTRA_MAX_CHARS)
}

/// `text` cut to `max` characters.
pub fn cut_to(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_owned(),
        None => text.to_owned(),
    }
}

/// A JSON value made safe for `extra`: every string cut to [`EXTRA_MAX_CHARS`], every array cut
/// to [`EXTRA_MAX_ITEMS`] elements, recursively.
pub fn cut_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(cut(&s)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .take(EXTRA_MAX_ITEMS)
                .map(cut_value)
                .collect(),
        ),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, cut_value(v))).collect())
        }
        other => other,
    }
}

/// Whether a key name looks like it holds a secret (case-insensitive: password, passwd, secret,
/// token, api key, private key, credential). Parsers never store the scalar value of such a key.
pub fn is_secret_key(name: &str) -> bool {
    let compact: String = name
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "privatekey",
        "credential",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
}

/// Number of lines as an editor shows them: 0 for empty text, otherwise newlines plus one unless
/// the text ends with a newline.
pub fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|b| *b == b'\n').count();
    let lines = if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    };
    u32::try_from(lines).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_splits_words_case_and_acronyms() {
        assert_eq!(tokenize("HTTPClient"), ["http", "client"]);
        assert_eq!(tokenize("get_user_name"), ["get", "user", "name"]);
        assert_eq!(tokenize("getUserName"), ["get", "user", "name"]);
        assert_eq!(tokenize("a/b.c::x-y,zz"), ["zz"]);
        assert_eq!(
            tokenize("g/a/app.py::Function::handler"),
            ["app", "py", "function", "handler"]
        );
        assert_eq!(
            tokenize("aws_s3_bucket.logs"),
            ["aws", "s3", "bucket", "logs"]
        );
        assert_eq!(tokenize("Kind/name"), ["kind", "name"]);
        assert!(tokenize("  ").is_empty());
        assert!(tokenize("x").is_empty());
    }

    #[test]
    fn cutting_and_secrets() {
        assert_eq!(cut(&"é".repeat(300)).chars().count(), 200);
        assert_eq!(cut("short"), "short");
        let v = cut_value(serde_json::json!({"a": ["x".repeat(500)], "n": 3}));
        assert_eq!(v["a"][0].as_str().unwrap().len(), 200);
        assert_eq!(v["n"], 3);
        for key in [
            "password",
            "DB_PASSWD",
            "clientSecret",
            "api-key",
            "API_KEY",
            "private_key",
            "token",
            "credentials",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in ["name", "image", "replicas", "tokenizer_mode_x"] {
            assert_eq!(is_secret_key(key), key.contains("token"), "{key}");
        }
    }

    #[test]
    fn lines_are_counted_like_an_editor() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\n\n"), 2);
    }
}

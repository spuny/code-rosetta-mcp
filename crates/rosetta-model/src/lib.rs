//! The shared vocabulary of Code Rosetta: graph kinds, what a parser produces, the API's JSON
//! shapes (DTOs) and the tool request/result types. No I/O.
//!
//! Repository filters (`repo` parameters) are globs on the repository name with `globset`
//! semantics and `literal_separator(true)`: `*` stays inside one path segment, `**` crosses
//! segments; a name without glob characters matches only that repository.
pub mod dto;
pub mod kinds;
pub mod parsed;
pub mod text;
pub mod tools;

pub use dto::{EdgeDto, HitDto, NodeDto, qualified_name, web_url};
pub use kinds::{EdgeKind, Language, NodeKind, QueryPattern, RefScheme, UnknownName};
pub use parsed::{Edge, Extra, MAX_NAME_CHARS, Node, ParsedFile, Ref, Target};
pub use text::{cut, cut_to, cut_value, is_secret_key, line_count, tokenize};

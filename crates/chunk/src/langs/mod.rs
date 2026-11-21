//! One module per language: a `Chunker` of its own, or a `Profile` behind the
//! shared pipeline of [`crate::brace`].

pub mod axle;
pub mod go;
pub mod java;
pub mod javascript;
pub mod json;
pub mod kotlin;
pub mod markdown;
pub mod php;
pub mod python;
pub mod rust;
pub mod text;

pub use axle::AxleChunker;
pub use go::GoChunker;
pub use java::JavaChunker;
pub use javascript::JsChunker;
pub use json::JsonChunker;
pub use kotlin::KotlinChunker;
pub use markdown::MarkdownChunker;
pub use php::PhpChunker;
pub use python::PythonChunker;
pub use rust::RustChunker;
pub use text::TextChunker;

/// The name of the module a file is: its stem, `app/rate_limit.py` →
/// `rate_limit` — or, for a file that stands for its directory (`mod.rs`,
/// `lib.rs`, `__init__.py`, listed in `directory_files`), that directory's
/// name, a crate's `src` skipped: `crates/core/src/lib.rs` → `core`.
///
/// INTENT: thirty-three files all named `mod` say nothing; the directory each
/// one stands for is the name its code is imported by.
pub(crate) fn module_name(path: &str, extension: &str, directory_files: &[&str]) -> String {
    let mut segments = path.rsplit('/');
    let file = segments.next().unwrap_or(path);
    let stem = file.strip_suffix(extension).unwrap_or(file);
    if !directory_files.contains(&stem) {
        return stem.to_string();
    }
    segments
        .find(|dir| !matches!(*dir, "" | "." | "src"))
        .unwrap_or(stem)
        .to_string()
}

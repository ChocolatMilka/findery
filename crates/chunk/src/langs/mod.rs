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

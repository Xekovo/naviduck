//! Document collection viewer/editor and command console for document databases (MongoDB),
//! 142-mongodb.md §5.2–5.3.

rust_i18n::i18n!("locales", fallback = "en");

pub mod console;
pub mod viewer;

pub use console::open_doc_console;
pub use viewer::{CollectionViewer, ViewMode, open_collection_viewer};

#[cfg(test)]
mod viewer_tests;

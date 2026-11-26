//! Key data editor and command console for key-value databases (Redis), 141-redis.md §5.2–5.3.

rust_i18n::i18n!("locales", fallback = "en");

pub mod editor;

pub use editor::{KeyEditor, open_key_editor};

//! Redis driver for naviduck: `Driver`/`Session`/`KeyValue` over fred, without GPUI.

pub mod config;
pub mod driver;
mod mapping;

pub use driver::RedisDriver;

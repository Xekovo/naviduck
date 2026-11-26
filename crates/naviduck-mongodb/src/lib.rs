//! MongoDB driver for naviduck: `Driver`/`Session`/`Document` over the official `mongodb`
//! crate, without GPUI.

mod config;
mod driver;
mod mapping;

pub use driver::MongoDriver;

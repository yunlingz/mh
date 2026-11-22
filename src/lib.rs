//! Minimal coding-agent primitives, separated from the terminal interface.

pub mod agent;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod context;
pub mod features;
pub mod message;
pub mod print;
pub mod providers;
pub(crate) mod regular_file;
pub mod runtime;
pub mod session;
pub mod setup;
pub(crate) mod signals;
pub mod storage;
pub mod text;
pub mod theme;
pub mod tools;
pub mod ui;

#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

//! This is a library so that it can be used from e2e tests without having to
//! spawn a process.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod domain;
pub mod infra;
mod run;
pub mod util;

pub use run::{main, run};

#[cfg(test)]
mod tests;

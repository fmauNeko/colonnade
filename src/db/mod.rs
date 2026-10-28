//! Database core: model types, SQL builder, backend traits and runtime bridge.

pub mod backend;
pub mod factory;
#[cfg(test)]
pub mod fake;
pub mod model;
pub mod mysql;
pub mod postgres;
pub mod runtime;
pub mod sql;
pub mod sqlite;

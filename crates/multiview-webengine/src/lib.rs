//! Bounded web capture and immutable artifact handoff for management workers.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
pub mod artifact;

pub mod cdp_wire;

pub mod capture;
pub mod fetch;
pub mod resource;
pub mod runner;
pub mod scheduler;
pub mod service;
pub mod service_journal;

//! nexuslink — local gateway: Anthropic /v1/messages in, OpenAI
//! /v1/chat/completions out. Library surface shared by the binary
//! and integration tests.

pub mod config;
pub mod desktop;
pub mod gateway;
pub mod mcp;
pub mod metrics;
pub mod proxy;
pub mod rotation;
pub mod streaming;
pub mod translator;
pub mod ui;

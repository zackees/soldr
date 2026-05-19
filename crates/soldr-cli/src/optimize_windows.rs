//! Scaffolding for `soldr optimize` Windows action layer. RED commit
//! — real implementation lands with the GREEN feat commit.

#![allow(dead_code)]

pub(crate) const ELEVATED_HELPER_FLAG: &str = "--as-elevated-helper";
pub(crate) const SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV: &str = "SOLDR_OPTIMIZE_HELPER_OUTPUT";
pub(crate) const SOLDR_TEST_DEFENDER_LOG_ENV: &str = "SOLDR_TEST_DEFENDER_LOG";
pub(crate) const SOLDR_TEST_ASSUME_ADMIN_ENV: &str = "SOLDR_TEST_ASSUME_ADMIN";
pub(crate) const SOLDR_TEST_DEFENDER_EXISTING_ENV: &str = "SOLDR_TEST_DEFENDER_EXISTING";

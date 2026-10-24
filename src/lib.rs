//! Library surface of EsoExtractData: the command-line definitions, LANG helpers and the legacy
//! MNF/DAT exporter, so other crates can depend on them. The `eso_extract_data` binary is a thin
//! wrapper around these modules.

pub mod cli;
pub mod lang_tools;
pub mod legacy;

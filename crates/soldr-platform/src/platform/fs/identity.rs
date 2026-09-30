//! Stable file identity and same-file comparison.

pub use crate::platform_imp::fs::identity::{
    file_identity, hardlink_identity, same_file, FileIdentity, HardlinkIdentity,
};

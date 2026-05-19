//! Scaffolding for `soldr optimize` detection. RED commit — real
//! implementation lands with the GREEN feat commit.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows10,
    Windows11Pre22H2,
    Windows11Post22H2,
    MacOS,
    Linux,
    Other,
}

pub(crate) fn parse_windows_build(_major: u32, _minor: u32, _build: u32) -> Platform {
    Platform::Other
}

pub(crate) fn detect_ci() -> Option<&'static str> {
    None
}

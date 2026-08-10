use std::path::Path;

/// Direct deps the v1 broker must not pull in. `ureq` is excluded
/// because #445 / #446 introduced it as a Windows-only fetch path
/// for the ConPTY sidecar — see `docs/v1-dependency-surface.md` for
/// the documented carve-out. All other entries remain forbidden so
/// the broker wire stays local-IPC-only.
const FORBIDDEN_DIRECT_DEPS: &[&str] = &[
    "axum",
    "curl",
    "h2",
    "http",
    "hyper",
    "isahc",
    "native-tls",
    "openssl",
    "quinn",
    "reqwest",
    "rustls",
    "surf",
    "tokio-tungstenite",
    "tonic",
    "tungstenite",
    "warp",
];

#[test]
fn broker_crate_has_no_direct_network_or_tls_dependencies() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let raw = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest.display()));
    let direct_dependencies = direct_dependency_names(&raw);
    let forbidden = direct_dependencies
        .iter()
        .filter(|name| FORBIDDEN_DIRECT_DEPS.contains(&name.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        forbidden.is_empty(),
        "running-process v1 broker must stay local-IPC-only; forbidden direct deps: {forbidden:?}"
    );
}

fn direct_dependency_names(manifest: &str) -> Vec<String> {
    let mut section = Section::Other;
    let mut names = Vec::new();

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = Section::from_header(trimmed);
            continue;
        }

        if !section.is_direct_dependencies() || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(name) = trimmed.split_once('=').map(|(name, _)| name.trim()) {
            names.push(name.trim_matches('"').to_owned());
        }
    }

    names
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    DirectDependencies,
    TargetDirectDependencies,
    Other,
}

impl Section {
    fn from_header(header: &str) -> Self {
        if header == "[dependencies]" {
            return Self::DirectDependencies;
        }
        if header.starts_with("[target.") && header.ends_with(".dependencies]") {
            return Self::TargetDirectDependencies;
        }
        Self::Other
    }

    fn is_direct_dependencies(self) -> bool {
        matches!(
            self,
            Self::DirectDependencies | Self::TargetDirectDependencies
        )
    }
}

#[cfg(test)]
mod tests {
    use super::direct_dependency_names;

    #[test]
    fn parser_ignores_dev_and_build_dependencies() {
        let names = direct_dependency_names(
            r#"
            [dependencies]
            prost = "0.14"

            [build-dependencies]
            reqwest = "0.12"

            [dev-dependencies]
            hyper = "1"

            [target.'cfg(windows)'.dependencies]
            winapi = "0.3"
            "#,
        );

        assert_eq!(names, ["prost", "winapi"]);
    }
}

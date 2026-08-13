// The boundary's inside: host cfg in a file named like a concrete-tree
// member is allowed, and feature/test cfg without a host selector is
// allowed anywhere.

#[cfg(target_os = "linux")]
mod selected {
    pub fn f() -> u8 {
        1
    }
}

#[cfg(feature = "tokio-console")]
fn feature_gated() {}

#[cfg(test)]
mod tests {
    #[test]
    fn neutral() {
        assert_eq!(super::selected::f(), 1);
    }
}

fn main() {}

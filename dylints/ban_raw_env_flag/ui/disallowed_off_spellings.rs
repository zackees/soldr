// The off-spelling half. soldr#2740's worst case was here: a hand-rolled
// off-set made `SOLDR_USE_SYSTEM_CMAKE=false` *enable* a switch that routes
// around the pinned, sha256-verified SDK.

fn disabled(value: &str) -> bool {
    matches!(value.trim(), "0" | "false" | "no" | "off")
}

fn main() {
    let _ = disabled("false");
}

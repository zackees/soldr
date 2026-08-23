// The lint must stay silent on code that does not hand-roll a spelling set.
//
// This file is named in `in_scope` as the one `ui/` file that is exempt, so it
// also proves that exemption works: without it, the fixture below would be
// judged like any other in-scope source.

/// A single accepted spelling is not an alternation, so nothing fires.
fn enabled(value: &str) -> bool {
    matches!(value.trim(), "1")
}

/// Matching on domain values that happen to sit beside each other is also
/// fine -- the lint keys on flag *spellings*, not on `|` alone.
fn mode(value: &str) -> u8 {
    match value {
        "sysroot" | "bundled" | "system" => 1,
        _ => 0,
    }
}

fn main() {
    let _ = enabled("1");
    let _ = mode("system");
}

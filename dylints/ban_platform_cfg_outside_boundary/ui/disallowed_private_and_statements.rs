// A cfg! in a private function body was invisible to the old item-only lint.
fn cfg_macro_in_private_body() -> u8 {
    if cfg!(unix) { 1 } else { 2 }
}

fn main() {}

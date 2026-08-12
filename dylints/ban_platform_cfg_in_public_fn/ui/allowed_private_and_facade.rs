#[cfg(unix)]
fn platform_value() -> u8 {
    1
}

#[cfg(windows)]
fn platform_value() -> u8 {
    2
}

pub fn platform_neutral_facade() -> u8 {
    platform_value()
}

fn private_platform_selection() -> u8 {
    if cfg!(windows) { 2 } else { 1 }
}

pub(self) fn explicitly_private_selection() -> u8 {
    if cfg!(windows) { 2 } else { 1 }
}

#[cfg(test)]
pub fn test_only_selection() -> u8 {
    if cfg!(windows) { 2 } else { 1 }
}

fn main() {
    let _ = (
        platform_neutral_facade(),
        private_platform_selection(),
        explicitly_private_selection(),
    );
}

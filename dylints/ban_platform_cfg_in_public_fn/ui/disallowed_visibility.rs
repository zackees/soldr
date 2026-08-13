// normalize-stderr-test: "\nerror: aborting due to 5 previous errors\n\n" -> ""
pub fn public_platform_selection() -> u8 {
    if cfg!(windows) { 2 } else { 1 }
}

#[cfg(unix)]
pub fn outer_attribute_selection() -> u8 {
    1
}

pub(crate) fn crate_platform_selection() -> u8 {
    #[cfg(unix)]
    return 1;
    #[cfg(windows)]
    return 2;
}

mod nested {
    pub(super) fn parent_platform_selection() -> u8 {
        if cfg!(target_os = "linux") { 1 } else { 2 }
    }

    pub(in crate) fn path_platform_selection() -> u8 {
        #[cfg_attr(windows, allow(dead_code))]
        let value = 1;
        value
    }
}

fn main() {
    let _ = (
        public_platform_selection(),
        outer_attribute_selection(),
        crate_platform_selection(),
        nested::parent_platform_selection(),
        nested::path_platform_selection(),
    );
}

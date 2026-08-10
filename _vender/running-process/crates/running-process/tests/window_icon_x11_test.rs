//! The X11 `_NET_WM_ICON` write path, against a real X server (#577).
//!
//! Unit tests cover decoding and cardinal packing. This covers the part that
//! cannot be faked: that the property actually lands on a window and reads
//! back byte-for-byte. It needs a display, so it skips without one — under
//! Xvfb in CI and in a container it runs for real.

#![cfg(target_os = "linux")]

use running_process::{set_icon, IconScope, IconSource};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, WindowClass};

/// A 2x1 solid PNG, so the expected pixels are trivially predictable.
fn png_2x1(colour: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, 2, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let row: Vec<u8> = colour.iter().copied().cycle().take(6).collect();
        writer.write_image_data(&row).expect("png data");
    }
    out
}

#[test]
fn the_icon_property_lands_on_the_window_and_reads_back_intact() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipping: no DISPLAY (run under Xvfb to exercise this)");
        return;
    }

    let (connection, screen_number) = x11rb::connect(None).expect("connect to X");
    let screen = &connection.setup().roots[screen_number];
    let window = connection.generate_id().expect("window id");
    connection
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            64,
            64,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new(),
        )
        .expect("create window")
        .check()
        .expect("window created");

    // The backend finds its target through WINDOWID, exactly as it would in a
    // terminal that exported one.
    // SAFETY: single-threaded test process.
    unsafe { std::env::set_var("WINDOWID", window.to_string()) };

    let png = png_2x1([0x12, 0x34, 0x56]);
    set_icon(IconScope::Host, &IconSource::Bytes(png)).expect("the icon must be written");

    let atom = connection
        .intern_atom(false, b"_NET_WM_ICON")
        .expect("intern")
        .reply()
        .expect("intern reply")
        .atom;
    let property = connection
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1024)
        .expect("get_property")
        .reply()
        .expect("property reply");

    let values: Vec<u32> = property.value32().expect("32-bit property").collect();
    // width, height, then one ARGB cardinal per pixel.
    assert_eq!(
        values,
        vec![2, 1, 0xff12_3456, 0xff12_3456],
        "the property must round-trip as [w, h, ARGB...]"
    );

    unsafe { std::env::remove_var("WINDOWID") };
}

#[test]
fn without_a_windowid_the_host_is_degraded_rather_than_available() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipping: no DISPLAY");
        return;
    }
    // SAFETY: single-threaded test process.
    unsafe { std::env::remove_var("WINDOWID") };
    let support = running_process::host_icon_support();
    assert!(
        !support.is_available(),
        "without a WINDOWID there is no window to target, so a real icon cannot be promised"
    );
    assert!(
        support.reason().is_some_and(|r| r.contains("WINDOWID")),
        "the reason must name what is missing; got {:?}",
        support.reason()
    );
}

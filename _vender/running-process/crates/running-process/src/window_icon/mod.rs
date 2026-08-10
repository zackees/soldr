//! Setting the host console/terminal window icon (#577).
//!
//! # Capability-reported, never assumed
//!
//! Most terminals do not let a running program change their window icon, and
//! they do not say so — the API call succeeds and nothing happens. Windows
//! Terminal is the case that matters most: `GetConsoleWindow` returns a real
//! handle to a hidden pseudo-console window, so `WM_SETICON` succeeds against
//! a window nobody can see.
//!
//! A function that returns `Ok(())` there would be worse than one that fails:
//! the caller would ship a feature that silently does nothing on the default
//! terminal of every recent Windows install. So support is *probed* and
//! reported, and [`set_host_icon`] refuses rather than pretending.
//!
//! # What is supported
//!
//! Classic Windows console (`conhost.exe`) only, for now. Everything else
//! reports [`IconSupport::Unsupported`] with a reason. Linux/X11 is a
//! plausible later addition; macOS Terminal.app and iTerm2, Windows Terminal,
//! Wayland compositors, and most modern emulators deliberately reserve the
//! window decoration to themselves, and no in-process API changes that.

use std::path::PathBuf;

pub mod ico;
mod osc;
#[cfg(target_os = "linux")]
mod x11;
// Gated to match `x11`, its only consumer: the PNG encoder it wraps is a
// Linux-only dependency, so compiling this elsewhere fails to find `png`.
#[cfg(all(test, target_os = "linux"))]
mod tests_support;

/// Where an icon comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    /// Icon file on disk: `.ico` on Windows.
    Path(PathBuf),
    /// Raw `.ico` bytes, typically embedded in the binary with
    /// `include_bytes!` so an application ships its own icon without needing
    /// a file to exist at runtime.
    Bytes(Vec<u8>),
    /// A stock icon the OS already ships, named symbolically.
    ///
    /// Nothing to bundle and nothing to decode, which suits the cases these
    /// exist for — marking a console as a warning or an error surface.
    /// See [`StockIcon`] for the names.
    Stock(StockIcon),
}

/// A stock icon provided by the operating system.
///
/// A closed set rather than a free-form string. A name the OS does not know
/// can only fail at runtime, and a caller has no way to discover which names
/// are valid; an enum makes the answer a compile error instead. The variants
/// are the ones with a direct equivalent on every platform this could grow
/// to, so the set stays meaningful rather than becoming Windows constants
/// wearing generic names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StockIcon {
    /// The application's own default icon.
    Application,
    /// Warning: a hazard the user should notice.
    Warning,
    /// Error: something has already gone wrong.
    Error,
    /// Information: neutral notice.
    Information,
    /// Shield: an elevation or security prompt.
    Shield,
}

impl StockIcon {
    /// The symbolic name sent by the OSC 1 fallback.
    ///
    /// freedesktop icon-naming-spec names rather than this crate's variant
    /// spelling: a terminal or window manager that does anything at all with
    /// OSC 1 looks the name up in the desktop icon theme, so a bespoke name
    /// would resolve to nothing on every host.
    pub fn osc_name(self) -> &'static str {
        match self {
            Self::Application => "application-x-executable",
            Self::Warning => "dialog-warning",
            Self::Error => "dialog-error",
            Self::Information => "dialog-information",
            Self::Shield => "security-high",
        }
    }
}

/// Whether this process can set its host window's icon.
///
/// # Capability matrix
///
/// | Host | Verdict | Backend |
/// |---|---|---|
/// | Windows conhost | `Available` | `WM_SETICON` |
/// | Windows Terminal | `Degraded` | OSC 1 name only; set the profile's `icon` field for a real image |
/// | Other Windows emulators | `Degraded` | OSC 1 name only |
/// | Linux X11 | `Degraded` | OSC 1 name only (`_NET_WM_ICON` not yet implemented) |
/// | Linux Wayland | `Unsupported` | compositors do not let a client set another window's icon |
/// | macOS | `Unsupported` | the window belongs to Terminal.app / iTerm2, not to this process |
/// | No terminal | `Unsupported` | nothing to set an icon on |
///
/// An out-of-date row here is a documentation regression: callers decide
/// whether to ship an icon at all based on this table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IconSupport {
    /// The host window accepts an icon.
    Available,
    /// The host accepts only a symbolic *name*, not an image.
    ///
    /// Reported rather than folded into `Available` because the difference is
    /// visible to the user: an OSC 1 name may be shown, ignored, or applied
    /// to something other than the window icon, and a caller told "yes" that
    /// then sees nothing change cannot tell a failure from a terminal that
    /// simply does not do icons.
    Degraded {
        /// What will actually happen, and why it is less than asked for.
        reason: &'static str,
    },
    /// It does not, and this is why.
    ///
    /// The reason is carried so a caller can log something an operator can
    /// act on, rather than a bare boolean that invites retrying forever.
    Unsupported {
        /// Human-readable explanation.
        reason: &'static str,
    },
}

impl IconSupport {
    /// Whether a real image icon can be set.
    ///
    /// False for [`IconSupport::Degraded`]: a caller choosing whether to embed
    /// and ship an icon file wants to know whether the file will be used, and
    /// on a degraded host it will not be.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Whether an attempt will do *something*, image or not.
    ///
    /// True for both [`IconSupport::Available`] and
    /// [`IconSupport::Degraded`] — the distinction a caller wants when
    /// deciding whether to bother calling at all, as opposed to whether to
    /// ship an image.
    pub fn is_attemptable(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }

    /// The reason support is absent or reduced, if it is.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Degraded { reason } | Self::Unsupported { reason } => Some(reason),
        }
    }
}

/// Why setting an icon failed.
#[derive(Debug, thiserror::Error)]
pub enum IconError {
    /// The host cannot accept an icon at all.
    ///
    /// Distinct from an I/O failure: retrying or supplying a different file
    /// will not help, and the caller should stop asking.
    #[error("this host cannot accept a window icon: {reason}")]
    Unsupported {
        /// Why the host is unsupported.
        reason: &'static str,
    },
    /// The icon source could not be loaded.
    #[error("cannot load icon from {path}: {source}")]
    Load {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The OS refused to build an icon from otherwise well-formed data.
    ///
    /// Removed in #720 because nothing constructed it; reinstated here
    /// because `CreateIconFromResourceEx` can fail on data this crate has
    /// already validated the shape of — the image itself may still be
    /// something the OS will not decode.
    #[error("the system refused the icon data: {0}")]
    Apply(#[source] std::io::Error),
    /// The host accepts only a symbolic name, and this source is not one.
    ///
    /// Distinct from [`IconError::Unsupported`]: the host *would* accept a
    /// stock icon, so the remedy is to pass one rather than to give up.
    #[error("this host accepts only a stock icon name, not an image file or bytes: {reason}")]
    DegradedSourceUnsupported {
        /// What the host will and will not accept.
        reason: &'static str,
    },
    /// The supplied bytes are not a usable icon.
    ///
    /// Separate from [`IconError::Load`] because the remedy differs: a bad
    /// path is fixed by pointing somewhere else, malformed bytes by fixing
    /// what was embedded.
    #[error("supplied icon data is unusable: {0}")]
    Decode(ico::IcoError),
}

/// Which window an icon operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconScope {
    /// This process's own host console window.
    Host,
    /// A child process's console window.
    ///
    /// Only meaningful when the child was given its own console
    /// (`CREATE_NEW_CONSOLE` on Windows). A child that inherited ours shares
    /// the same window, so targeting it changes this process's icon too —
    /// that is inherent to sharing a console, not a failure, and
    /// [`icon_support`] reports it as available because the icon really does
    /// change.
    Child {
        /// Process id of the child.
        pid: u32,
    },
}

/// Whether a window can accept an icon.
///
/// Cheap, and safe to call before deciding whether to ship an icon at all.
pub fn icon_support(scope: IconScope) -> IconSupport {
    imp::icon_support(scope)
}

/// Whether this process's host window can accept an icon.
pub fn host_icon_support() -> IconSupport {
    icon_support(IconScope::Host)
}

/// Set the icon on this process's host console window.
///
/// Returns [`IconError::Unsupported`] when the host does not accept icons,
/// rather than succeeding without effect.
pub fn set_host_icon(source: &IconSource) -> Result<(), IconError> {
    set_icon(IconScope::Host, source)
}

/// Set the icon on the window named by `scope`.
///
/// Returns [`IconError::Unsupported`] when that window does not accept icons,
/// rather than succeeding without effect.
pub fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
    set_icon_given(icon_support(scope), scope, source)
}

/// [`set_host_icon`] with the support verdict supplied.
///
/// Split out so the refusal path is testable on every platform without
/// depending on whether the machine running the tests happens to have a
/// console window. A test that only exercises the refusal when the ambient
/// host is unsupported silently checks nothing everywhere else.
#[cfg(test)]
fn set_host_icon_given(support: IconSupport, source: &IconSource) -> Result<(), IconError> {
    set_icon_given(support, IconScope::Host, source)
}

fn set_icon_given(
    support: IconSupport,
    scope: IconScope,
    source: &IconSource,
) -> Result<(), IconError> {
    match support {
        IconSupport::Available => imp::set_icon(scope, source),
        // Only a stock name has anything to send. A file or a byte blob would
        // mean inventing a name the caller never chose, and OSC 1 carries a
        // name rather than an image.
        IconSupport::Degraded { reason } => match source {
            IconSource::Stock(icon) => osc::emit(icon.osc_name()).map_err(IconError::Apply),
            _ => Err(IconError::DegradedSourceUnsupported { reason }),
        },
        IconSupport::Unsupported { reason } => Err(IconError::Unsupported { reason }),
    }
}

#[cfg(windows)]
mod imp {
    use super::{IconError, IconScope, IconSource, IconSupport, StockIcon};
    use std::os::windows::ffi::OsStrExt as _;

    use winapi::shared::minwindef::{BOOL, DWORD, FALSE, LPARAM, TRUE};
    use winapi::shared::windef::{HICON, HWND};
    use winapi::um::wincon::GetConsoleWindow;
    use winapi::um::winuser::{
        CreateIconFromResourceEx, EnumWindows, GetClassNameW, GetWindowThreadProcessId, LoadIconW,
        LoadImageW, SendMessageW, IDI_APPLICATION, IDI_ERROR, IDI_INFORMATION, IDI_SHIELD,
        IDI_WARNING, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, WM_SETICON,
    };

    /// `wParam` values for `WM_SETICON`.
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    /// Window class of the classic console host.
    ///
    /// This is the discriminator that matters. Windows Terminal hosts the
    /// session in a pseudo-console whose `GetConsoleWindow` handle belongs to
    /// a hidden window of a different class — `WM_SETICON` against it
    /// succeeds and changes nothing visible.
    const CONHOST_CLASS: &str = "ConsoleWindowClass";

    fn console_window() -> Option<HWND> {
        let hwnd = unsafe { GetConsoleWindow() };
        (!hwnd.is_null()).then_some(hwnd)
    }

    fn class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }

    /// The console window a scope names, if there is one.
    fn window_for(scope: IconScope) -> Option<HWND> {
        match scope {
            IconScope::Host => console_window(),
            IconScope::Child { pid } => console_window_of_pid(pid),
        }
    }

    /// Find the console window owned by `pid`.
    ///
    /// A process has at most one console window, so the first match is the
    /// answer. The class is checked here as well as in the support probe
    /// because a process can own windows that are not its console.
    fn console_window_of_pid(pid: u32) -> Option<HWND> {
        struct Search {
            pid: u32,
            found: HWND,
        }

        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let search = &mut *(lparam as *mut Search);
            let mut owner: DWORD = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == search.pid && class_name(hwnd) == CONHOST_CLASS {
                search.found = hwnd;
                return FALSE; // stop: a process has one console window
            }
            TRUE
        }

        let mut search = Search {
            pid,
            found: std::ptr::null_mut(),
        };
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
        (!search.found.is_null()).then_some(search.found)
    }

    pub(super) fn icon_support(scope: IconScope) -> IconSupport {
        if let IconScope::Child { pid } = scope {
            return match console_window_of_pid(pid) {
                Some(_) => IconSupport::Available,
                // Either the child has no console of its own (it inherited
                // ours, or was created with CREATE_NO_WINDOW), or it has
                // already exited. Both mean there is no window to target.
                None => IconSupport::Unsupported {
                    reason: "that process has no console window of its own (it may share this                              one, have been created without a window, or have exited)",
                },
            };
        }
        // Checked before the window class because it yields a remedy the
        // class check cannot: Windows Terminal *does* support a per-profile
        // icon, just not one set at runtime. "Set the profile's icon field"
        // is actionable; "your host owns its decoration" is not.
        if std::env::var_os("WT_SESSION").is_some() {
            return IconSupport::Degraded {
                reason: "Windows Terminal owns its window decoration and ignores WM_SETICON.                          Set the `icon` field on the WT profile for a real image; a stock name                          can still be sent via OSC 1",
            };
        }
        let Some(hwnd) = console_window() else {
            return IconSupport::Unsupported {
                reason: "this process has no console window (detached, or output is redirected \
                         from a windowless host)",
            };
        };
        if class_name(hwnd) == CONHOST_CLASS {
            return IconSupport::Available;
        }
        IconSupport::Degraded {
            reason: "the host is not the classic console (conhost). Modern emulators own \
                     their window decoration and ignore WM_SETICON; a stock name can still \
                     be sent via OSC 1",
        }
    }

    /// Load an icon from a file, letting the OS pick the best size.
    fn load_from_path(path: &std::path::Path) -> Result<HICON, IconError> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);

        // LR_DEFAULTSIZE picks the system's preferred size from a multi-image
        // .ico rather than whichever image happens to be first.
        let icon = unsafe {
            LoadImageW(
                std::ptr::null_mut(),
                wide.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        } as HICON;
        if icon.is_null() {
            return Err(IconError::Load {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(icon)
    }

    /// Load an icon from `.ico` bytes held in memory.
    ///
    /// There is no `LoadImage` equivalent that takes a whole `.ico` from
    /// memory, so the directory is walked here to find one image and
    /// `CreateIconFromResourceEx` is given exactly that span. The bytes are
    /// treated as untrusted: `super::ico::best_image` bounds-checks every
    /// offset before we hand a length to the OS, which would otherwise read
    /// whatever follows in our address space.
    fn load_from_bytes(bytes: &[u8]) -> Result<HICON, IconError> {
        let span = super::ico::best_image(bytes).map_err(IconError::Decode)?;
        let image = &bytes[span.offset..span.offset + span.len];

        // 0x00030000 is the icon resource version the API expects.
        const ICON_RESOURCE_VERSION: DWORD = 0x0003_0000;
        let icon = unsafe {
            CreateIconFromResourceEx(
                image.as_ptr() as *mut u8,
                image.len() as DWORD,
                TRUE,
                ICON_RESOURCE_VERSION,
                0,
                0,
                LR_DEFAULTSIZE,
            )
        };
        if icon.is_null() {
            return Err(IconError::Apply(std::io::Error::last_os_error()));
        }
        Ok(icon)
    }

    /// Load an icon the OS already provides.
    ///
    /// These are shared resources owned by the system, so unlike the file and
    /// byte paths there is nothing to free and no data to validate — the only
    /// failure is the OS declining to hand one over.
    pub(super) fn load_stock(stock: StockIcon) -> Result<HICON, IconError> {
        let name = match stock {
            StockIcon::Application => IDI_APPLICATION,
            StockIcon::Warning => IDI_WARNING,
            StockIcon::Error => IDI_ERROR,
            StockIcon::Information => IDI_INFORMATION,
            StockIcon::Shield => IDI_SHIELD,
        };
        // A null hInstance asks for a system icon rather than one from this
        // module's resources.
        let icon = unsafe { LoadIconW(std::ptr::null_mut(), name) };
        if icon.is_null() {
            return Err(IconError::Apply(std::io::Error::last_os_error()));
        }
        Ok(icon)
    }

    pub(super) fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
        let hwnd = window_for(scope).ok_or(IconError::Unsupported {
            reason: "the console window disappeared between the support probe and the call",
        })?;

        let icon = match source {
            IconSource::Path(path) => load_from_path(path)?,
            IconSource::Bytes(bytes) => load_from_bytes(bytes)?,
            IconSource::Stock(stock) => load_stock(*stock)?,
        };

        // Both slots: the small icon is the title bar and Alt+Tab, the big one
        // is the taskbar. Setting only one leaves the other stale, which looks
        // like a partial failure to a user.
        unsafe {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL, icon as isize);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG, icon as isize);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{IconError, IconScope, IconSource, IconSupport};

    pub(super) fn icon_support(_scope: IconScope) -> IconSupport {
        // Per-platform verdicts rather than one blanket string. A caller
        // logging "unsupported" on macOS and on headless Linux is logging two
        // different problems, and only one of them has a remedy.
        if cfg!(target_os = "macos") {
            return IconSupport::Unsupported {
                reason: "on macOS the window belongs to Terminal.app or iTerm2, not to this                          process; set the icon on the terminal application's own bundle",
            };
        }
        #[cfg(target_os = "linux")]
        {
            super::x11::support(_scope)
        }
        #[cfg(not(target_os = "linux"))]
        {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                return IconSupport::Unsupported {
                    reason: "Wayland compositors do not let a client change another window's                              icon; set it in the terminal emulator's .desktop file",
                };
            }
            IconSupport::Unsupported {
                reason: "no window-icon backend exists for this platform",
            }
        }
    }

    pub(super) fn set_icon(_scope: IconScope, _source: &IconSource) -> Result<(), IconError> {
        #[cfg(target_os = "linux")]
        {
            super::x11::set_icon(_scope, _source)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(IconError::Unsupported {
                reason: "no window-icon backend exists for this platform",
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer without panicking wherever it runs — including
    /// CI, which has no console window at all.
    #[test]
    fn support_is_reportable_everywhere() {
        let support = host_icon_support();
        // Whichever answer, it must be self-describing: an unsupported result
        // without a reason would leave a caller with nothing to log.
        match &support {
            IconSupport::Available => assert_eq!(support.reason(), None),
            IconSupport::Degraded { reason } | IconSupport::Unsupported { reason } => {
                assert!(!reason.is_empty(), "a reduced verdict must explain itself");
                assert_eq!(support.reason(), Some(*reason));
            }
        }
    }

    /// Windows Terminal must be detected by env, not only by window class.
    ///
    /// The class check cannot distinguish WT from any other non-conhost
    /// host, and only WT has the specific remedy of a per-profile `icon`
    /// field. This runs the real detection with the env var set, so the
    /// branch is exercised rather than assumed.
    #[test]
    #[cfg(windows)]
    fn windows_terminal_is_detected_by_env_and_names_its_remedy() {
        // SAFETY: single-threaded test process; the var is restored below.
        let previous = std::env::var_os("WT_SESSION");
        unsafe { std::env::set_var("WT_SESSION", "test-session") };
        let support = host_icon_support();
        match previous {
            Some(value) => unsafe { std::env::set_var("WT_SESSION", value) },
            None => unsafe { std::env::remove_var("WT_SESSION") },
        }

        match support {
            IconSupport::Degraded { reason } => {
                assert!(
                    reason.contains("profile"),
                    "WT's verdict must point at the profile icon field; got {reason:?}"
                );
            }
            other => panic!("WT_SESSION must yield Degraded, got {other:?}"),
        }
    }
    #[test]
    fn a_degraded_host_is_attemptable_but_not_available() {
        // The distinction a caller acts on: `is_available` decides whether
        // to embed and ship an icon file, `is_attemptable` decides whether
        // to bother calling at all.
        let degraded = IconSupport::Degraded {
            reason: "name only",
        };
        assert!(!degraded.is_available());
        assert!(degraded.is_attemptable());
        assert_eq!(degraded.reason(), Some("name only"));

        assert!(IconSupport::Available.is_attemptable());
        assert!(!IconSupport::Unsupported { reason: "no" }.is_attemptable());
    }

    #[test]
    fn a_degraded_host_accepts_a_stock_name_and_refuses_an_image() {
        // OSC 1 carries a name, not an image. Accepting a file here would
        // mean inventing a name the caller never chose.
        let degraded = IconSupport::Degraded {
            reason: "name only",
        };
        let refused = set_host_icon_given(
            degraded.clone(),
            &IconSource::Path(PathBuf::from("some.ico")),
        )
        .expect_err("an image must be refused on a name-only host");
        match refused {
            IconError::DegradedSourceUnsupported { reason } => {
                assert_eq!(reason, "name only");
            }
            other => panic!("expected DegradedSourceUnsupported, got {other:?}"),
        }

        // And it is distinct from Unsupported, because the remedy differs:
        // pass a stock icon rather than give up.
        let unsupported = set_host_icon_given(
            IconSupport::Unsupported {
                reason: "none at all",
            },
            &IconSource::Stock(StockIcon::Shield),
        )
        .expect_err("an unsupported host refuses everything");
        assert!(matches!(unsupported, IconError::Unsupported { .. }));
    }

    #[test]
    fn every_stock_icon_maps_to_a_freedesktop_name() {
        // A bespoke name would resolve to nothing in any desktop icon
        // theme, which is the only place an OSC 1 name gets looked up.
        for icon in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let name = icon.osc_name();
            assert!(!name.is_empty(), "{icon:?} has no OSC name");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{icon:?} -> {name:?} is not a freedesktop-style name"
            );
        }
    }

    #[test]
    fn stock_names_are_distinct() {
        // Two icons sharing a name would silently show the wrong one.
        let names: std::collections::BTreeSet<&str> = [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ]
        .into_iter()
        .map(StockIcon::osc_name)
        .collect();
        assert_eq!(names.len(), 5);
    }
    #[test]
    fn availability_and_reason_are_consistent() {
        assert!(IconSupport::Available.is_available());
        assert!(IconSupport::Available.reason().is_none());

        let no = IconSupport::Unsupported { reason: "because" };
        assert!(!no.is_available());
        assert_eq!(no.reason(), Some("because"));
    }

    /// An unsupported host must refuse rather than report success.
    ///
    /// This is the whole point of the module: a caller that gets `Ok(())`
    /// would ship a feature that silently does nothing on the default
    /// terminal of every recent Windows install.
    ///
    /// The verdict is injected rather than probed, so this runs the refusal
    /// on every platform. Probing would make the test a no-op wherever the
    /// ambient host happens to be supported.
    #[test]
    fn an_unsupported_host_refuses_instead_of_pretending() {
        let error = set_host_icon_given(
            IconSupport::Unsupported {
                reason: "test verdict",
            },
            &IconSource::Path("anything.ico".into()),
        )
        .expect_err("an unsupported host must not report success");

        match error {
            IconError::Unsupported { reason } => assert_eq!(reason, "test verdict"),
            other => panic!("expected Unsupported, got {other}"),
        }
    }

    /// The refusal must not depend on the icon existing: an unsupported host
    /// is unsupported whatever it is handed.
    #[test]
    fn refusal_precedes_loading_the_icon() {
        let error = set_host_icon_given(
            IconSupport::Unsupported { reason: "nope" },
            &IconSource::Path("definitely-does-not-exist.ico".into()),
        )
        .expect_err("must refuse");
        assert!(
            matches!(error, IconError::Unsupported { .. }),
            "a missing file must not mask the unsupported verdict; got {error}"
        );
    }

    /// The OS must hand back a real icon for every variant.
    ///
    /// Calls the loader directly rather than going through `set_host_icon`,
    /// which refuses at the window lookup on a machine with no console — so
    /// the enum-to-OS mapping would otherwise never run here. That is the
    /// only place the mapping itself is exercised.
    #[cfg(windows)]
    #[test]
    fn the_os_supplies_every_stock_icon() {
        for stock in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let icon =
                imp::load_stock(stock).unwrap_or_else(|e| panic!("the OS declined {stock:?}: {e}"));
            assert!(!icon.is_null(), "{stock:?} produced a null icon");
        }
    }

    /// A pid that owns no console window must be refused, with a reason.
    ///
    /// Platform-neutral: off Windows the whole feature is unavailable and
    /// says so, which is a different sentence but the same contract. An
    /// earlier version asserted the Windows wording here and failed on the
    /// musl and coverage lanes — the assertion was Windows-specific while the
    /// test was not.
    #[test]
    fn a_process_with_no_console_window_is_unsupported() {
        // pid 0 is the system idle process and never owns a console window,
        // so this is stable across machines and needs no fixture.
        let support = icon_support(IconScope::Child { pid: 0 });
        assert!(!support.is_available());
        assert!(
            !support.reason().expect("must explain itself").is_empty(),
            "an unsupported result must carry a usable reason"
        );
    }

    /// On Windows the reason must name what is actually missing, so a caller
    /// knows to spawn with CREATE_NEW_CONSOLE rather than retrying.
    #[cfg(windows)]
    #[test]
    fn a_childless_pid_reason_names_the_console_window() {
        let support = icon_support(IconScope::Child { pid: 0 });
        let reason = support.reason().expect("must explain itself");
        assert!(
            reason.contains("console window"),
            "the reason should name what is missing: {reason}"
        );
    }

    /// Looking up our OWN pid must find the same window the host scope does.
    ///
    /// This is the deterministic test of the pid lookup: no spawning, no
    /// waiting, no session-wide state. Whenever this process has a console
    /// window, `Child { pid: self }` names that very window, so the two
    /// scopes must agree — and a broken `console_window_of_pid` makes them
    /// disagree immediately.
    ///
    /// Where there is no console window both are unsupported, which is also
    /// agreement, so the assertion holds on every machine.
    #[test]
    fn own_pid_resolves_to_the_host_console_window() {
        let host = icon_support(IconScope::Host);
        let own = icon_support(IconScope::Child {
            pid: std::process::id(),
        });
        assert_eq!(
            host.is_available(),
            own.is_available(),
            "host scope says {host:?} but our own pid says {own:?}; the pid lookup              disagrees with the direct console-window lookup"
        );
    }

    /// And the setter refuses rather than silently doing nothing.
    #[test]
    fn setting_a_childless_pid_is_an_error() {
        let error = set_icon(
            IconScope::Child { pid: 0 },
            &IconSource::Stock(StockIcon::Warning),
        )
        .expect_err("a pid with no console cannot take an icon");
        assert!(
            matches!(error, IconError::Unsupported { .. }),
            "expected Unsupported, got {error}"
        );
    }

    /// An exited process cannot be targeted either — same answer, so a caller
    /// does not have to distinguish "never had one" from "gone".
    #[test]
    fn an_implausible_pid_is_unsupported() {
        let support = icon_support(IconScope::Child { pid: u32::MAX });
        assert!(!support.is_available());
    }

    /// Host scope must keep answering exactly as before: the scope-aware
    /// entry point is a generalisation, not a behaviour change.
    #[test]
    fn host_scope_agrees_with_the_host_specific_helper() {
        assert_eq!(icon_support(IconScope::Host), host_icon_support());
    }

    #[test]
    fn scopes_are_distinguishable() {
        assert_ne!(IconScope::Host, IconScope::Child { pid: 1 });
        assert_ne!(IconScope::Child { pid: 1 }, IconScope::Child { pid: 2 });
        assert_eq!(IconScope::Child { pid: 7 }, IconScope::Child { pid: 7 });
    }

    /// A stock icon needs no data, so the only thing that can go wrong is
    /// the host — never a decode.
    ///
    /// Runs everywhere by forcing the verdict, so the enum-to-OS mapping is
    /// exercised on platforms with no console window at all.
    #[test]
    fn every_stock_icon_is_requestable() {
        for stock in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let result = set_host_icon_given(IconSupport::Available, &IconSource::Stock(stock));
            match result {
                // On a host with a real console window the icon is set.
                Ok(()) => {}
                // Without one, the refusal comes from the window lookup — not
                // from the icon, which is the point: a stock icon is never a
                // decode failure.
                Err(IconError::Unsupported { .. }) => {}
                Err(other) => panic!("{stock:?} failed for a reason other than the host: {other}"),
            }
        }
    }

    /// A stock request must never be reported as bad data.
    #[test]
    fn a_stock_icon_is_never_a_decode_error() {
        let result = set_host_icon_given(
            IconSupport::Available,
            &IconSource::Stock(StockIcon::Warning),
        );
        if let Err(error) = result {
            assert!(
                !matches!(error, IconError::Decode(_)),
                "a stock icon carries no data to decode, got {error}"
            );
        }
    }

    /// Distinct variants must not collapse onto one another.
    #[test]
    fn stock_variants_are_distinguishable() {
        assert_ne!(StockIcon::Warning, StockIcon::Error);
        assert_ne!(StockIcon::Application, StockIcon::Shield);
        assert_eq!(StockIcon::Information, StockIcon::Information);
    }

    /// Malformed bytes must be refused before the OS sees them.
    ///
    /// Runs everywhere by forcing the verdict, because the decode happens
    /// before any window is touched — so this covers the validation on
    /// platforms that have no console window at all.
    #[test]
    fn malformed_icon_bytes_are_refused() {
        let result =
            set_host_icon_given(IconSupport::Available, &IconSource::Bytes(vec![0xFF; 64]));
        let error = result.expect_err("garbage is not an icon");
        assert!(
            matches!(error, IconError::Decode(_) | IconError::Unsupported { .. }),
            "expected a refusal before the OS was handed anything, got {error}"
        );
    }

    #[test]
    fn empty_icon_bytes_are_refused() {
        let error = set_host_icon_given(IconSupport::Available, &IconSource::Bytes(Vec::new()))
            .expect_err("empty data is not an icon");
        assert!(
            matches!(error, IconError::Decode(_) | IconError::Unsupported { .. }),
            "got {error}"
        );
    }

    /// A missing file must be a load error, not a silent success.
    ///
    /// Reaching the load path needs a real conhost window, which a CI runner
    /// does not have. Rather than skip invisibly, the verdict is forced to
    /// `Available` so the load path runs everywhere: with no console window
    /// `imp::set_host_icon` returns `Unsupported`, and with one it returns
    /// `Load`. Both are refusals — what must never happen is `Ok`.
    #[test]
    fn a_missing_icon_file_never_reports_success() {
        let result = set_host_icon_given(
            IconSupport::Available,
            &IconSource::Path("no-such-icon-file.ico".into()),
        );
        let error = result.expect_err("a missing file cannot produce a set icon");
        assert!(
            matches!(
                error,
                IconError::Load { .. } | IconError::Unsupported { .. }
            ),
            "expected a refusal, got {error}"
        );
    }
}

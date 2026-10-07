//! Fake Clippy tools that preserve compiler routing and child exit status.

use super::*;

pub(crate) fn fake_cargo_clippy_script(log_path: &Path, clippy_driver: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo wrapper=%RUSTC_WRAPPER% workspace_wrapper={1} rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% zccache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
             if \"%~1\"==\"clippy\" goto clippy\n\
             echo unsupported fake cargo invocation %* 1>&2\n\
             exit /b 1\n\
             :clippy\n\
             if not defined RUSTC_WRAPPER goto direct_clippy\n\
             call \"%RUSTC_WRAPPER%\" \"{1}\" \"%RUSTC%\" --crate-name demo --crate-type lib --emit metadata,dep-info src/lib.rs -o \"{2}\\libdemo.rmeta\" --out-dir \"{2}\"\n\
             exit /b %ERRORLEVEL%\n\
             :direct_clippy\n\
             call \"{1}\" \"%RUSTC%\" --crate-name demo --crate-type lib --emit metadata,dep-info src/lib.rs -o \"{2}\\libdemo.rmeta\" --out-dir \"{2}\"\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            clippy_driver.display(),
            output_dir.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} workspace_wrapper={1} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"clippy\" ]; then\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"{1}\" \"$RUSTC\" --crate-name demo --crate-type lib --emit metadata,dep-info src/lib.rs -o \"{2}/libdemo.rmeta\" --out-dir \"{2}\"\n\
               else\n\
                 \"{1}\" \"$RUSTC\" --crate-name demo --crate-type lib --emit metadata,dep-info src/lib.rs -o \"{2}/libdemo.rmeta\" --out-dir \"{2}\"\n\
               fi\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            clippy_driver.display(),
            output_dir.display()
        )
    }
}

pub(crate) fn fake_clippy_driver_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"rustc=%~1\"\n\
             set \"first_arg=%~1\"\n\
             if \"%first_arg:~0,1%\"==\"-\" set \"rustc=%SOLDR_TEST_RUSTC_BIN%\"\n\
             if not \"%first_arg:~0,1%\"==\"-\" shift\n\
             set \"args=\"\n\
             :collect_args\n\
             if \"%~1\"==\"\" goto run_clippy\n\
             set args=%args% \"%~1\"\n\
             shift\n\
             goto collect_args\n\
             :run_clippy\n\
             echo clippy-driver %rustc% %args%>>\"{}\"\n\
             call \"%rustc%\" %args%\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             rustc=\"$1\"\n\
             case \"$rustc\" in\n\
               -*) rustc=\"${{SOLDR_TEST_RUSTC_BIN:-${{RUSTC:-rustc}}}}\"; set -- \"$1\" \"$@\" ;;\n\
               *) shift ;;\n\
             esac\n\
             shift\n\
             echo \"clippy-driver $rustc $*\" >> \"{}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display()
        )
    }
}

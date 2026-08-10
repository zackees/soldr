from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class TinyPdbSymbolSpec:
    name: str
    source: str
    needle: str
    category: str


TINY_PDB_SYMBOLS: tuple[TinyPdbSymbolSpec, ...] = (
    TinyPdbSymbolSpec(
        "rp_native_process_start_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_process_start_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_process_wait_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_process_wait_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_process_kill_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_process_kill_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_process_close_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_process_close_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_process_read_combined_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_process_read_combined_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_process_wait_for_capture_completion_public",
        "crates/running-process/src/public_symbols.rs",
        "pub unsafe extern \"C\" fn rp_native_process_wait_for_capture_completion_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_assign_child_to_windows_kill_on_close_job_public",
        "crates/running-process/src/public_symbols.rs",
        "pub extern \"C\" fn rp_assign_child_to_windows_kill_on_close_job_public(",
        "core",
    ),
    TinyPdbSymbolSpec(
        "rp_native_apply_process_nice_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_apply_process_nice_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_windows_apply_process_priority_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_windows_apply_process_priority_public(",
        "win32",
    ),
    TinyPdbSymbolSpec(
        "rp_windows_generate_console_ctrl_break_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_windows_generate_console_ctrl_break_public(",
        "win32",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_start_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_start_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_wait_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_wait_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_kill_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_kill_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_terminate_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_terminate_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_close_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_close_public(",
        "api",
    ),
    TinyPdbSymbolSpec(
        "rp_native_running_process_send_interrupt_public",
        "crates/running-process-py/src/public_symbols.rs",
        "pub extern \"C\" fn rp_native_running_process_send_interrupt_public(",
        "api",
    ),
    # PTY public symbols moved to running-process/src/pty/
)

DISALLOWED_PUBLIC_SYMBOL_PATTERNS: tuple[str, ...] = (
    "pyo3",
    "sysinfo",
    "portable_pty",
    "windows_core",
    "regex",
    "aho_corasick",
    "rayon",
    "memchr",
    "anon.",
    ".llvm.",
)


def public_symbol_names() -> list[str]:
    return [spec.name for spec in TINY_PDB_SYMBOLS]


def filter_list_contents() -> str:
    return "".join(f"{name}\n" for name in public_symbol_names())

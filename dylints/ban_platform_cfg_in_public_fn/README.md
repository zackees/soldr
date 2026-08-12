# ban_platform_cfg_in_public_fn

Requires every function whose Rust visibility escapes its immediate module to
have a platform-neutral declaration and body. Such a function may call a
private adapter, but must not contain `#[cfg(...)]`, `#[cfg_attr(...)]`, or
`cfg!(...)` selecting an OS, family, architecture, ABI, vendor, endian, or
pointer width.

Existing debt is listed exactly in `src/allowlist.txt`. The repository ratchet
test requires that file to equal the current violation set, so new and stale
entries both fail CI. Move platform mechanics behind cfg-selected private
functions or modules with identical outer signatures, then delete the matching
allowlist line.

//! Does an ELF binary find its libraries relative to itself? (soldr#3403)
//!
//! `$ORIGIN` in `DT_RPATH` / `DT_RUNPATH` resolves against the directory of the
//! executable that names it, so an ELF whose rpath is `$ORIGIN/../x.libs`
//! (what maturin's repair writes for a bundled `liblzma`) breaks the moment it
//! is hardlinked or copied to another directory: the loader aborts before
//! `main` with `error while loading shared libraries`. This is the Linux
//! counterpart of the Mach-O `@loader_path` check in [`crate::self_relocate`].
//!
//! Only the program headers and the dynamic section are read -- never a string
//! search over the whole file, since `$ORIGIN` legitimately appears in `.rodata`
//! of unrelated binaries. Every read is bounds-checked, every loop is capped,
//! and anything unparseable answers `false`, which keeps today's hardlink path.

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
const MAX_PROGRAM_HEADERS: u64 = 128;
const MAX_DYNAMIC_ENTRIES: u64 = 4096;
const MAX_RPATH_BYTES: usize = 4096;

/// The dynamic-section strings that matter for relocation: the libraries an
/// ELF needs at load time, and its rpath / runpath entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DynamicStrings {
    pub(crate) needed: Vec<String>,
    pub(crate) rpaths: Vec<String>,
}

/// True when `bytes` is an ELF whose `DT_RPATH` or `DT_RUNPATH` contains
/// `$ORIGIN` / `${ORIGIN}`.
pub(crate) fn elf_has_origin_rpath(bytes: &[u8]) -> bool {
    dynamic_strings(bytes).is_some_and(|dynamic| {
        dynamic
            .rpaths
            .iter()
            .any(|text| text.contains("$ORIGIN") || text.contains("${ORIGIN}"))
    })
}

/// The `DT_NEEDED` library names of an ELF, or `None` when `bytes` is not a
/// parseable dynamic ELF.
pub(crate) fn elf_needed_libraries(bytes: &[u8]) -> Option<Vec<String>> {
    dynamic_strings(bytes).map(|dynamic| dynamic.needed)
}

struct Reader<'a> {
    bytes: &'a [u8],
    big_endian: bool,
    is64: bool,
}

impl Reader<'_> {
    fn uint(&self, offset: u64, size: usize) -> Option<u64> {
        let start = usize::try_from(offset).ok()?;
        let raw = self.bytes.get(start..start.checked_add(size)?)?;
        let mut value: u64 = 0;
        if self.big_endian {
            for byte in raw {
                value = (value << 8) | u64::from(*byte);
            }
        } else {
            for byte in raw.iter().rev() {
                value = (value << 8) | u64::from(*byte);
            }
        }
        Some(value)
    }

    fn word(&self, offset: u64) -> Option<u64> {
        self.uint(offset, if self.is64 { 8 } else { 4 })
    }
}

fn dynamic_strings(bytes: &[u8]) -> Option<DynamicStrings> {
    if bytes.get(..4)? != b"\x7fELF" {
        return None;
    }
    let is64 = match bytes.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    let big_endian = match bytes.get(5)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    let reader = Reader {
        bytes,
        big_endian,
        is64,
    };
    let word = if is64 { 8 } else { 4 };
    let (phoff_at, phentsize_at, phnum_at) = if is64 { (32, 54, 56) } else { (28, 42, 44) };
    let phoff = reader.word(phoff_at)?;
    let phentsize = reader.uint(phentsize_at, 2)?;
    let phnum = reader.uint(phnum_at, 2)?.min(MAX_PROGRAM_HEADERS);
    // p_type u32 first; then p_offset / p_vaddr / p_filesz at class-specific spots.
    let (off_at, vaddr_at, filesz_at, min_entry) = if is64 {
        (8, 16, 32, 56)
    } else {
        (4, 8, 16, 32)
    };
    if phentsize < min_entry {
        return None;
    }

    let mut loads: Vec<(u64, u64, u64)> = Vec::new();
    let mut dynamic: Option<(u64, u64)> = None;
    for index in 0..phnum {
        let base = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let kind = reader.uint(base, 4)?;
        let offset = reader.word(base + off_at)?;
        let vaddr = reader.word(base + vaddr_at)?;
        let filesz = reader.word(base + filesz_at)?;
        match u32::try_from(kind).ok()? {
            PT_LOAD => loads.push((vaddr, offset, filesz)),
            PT_DYNAMIC => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let (dyn_offset, dyn_size) = dynamic?;

    let entry = word * 2;
    let mut strtab_vaddr: Option<u64> = None;
    let mut rpath_offsets: Vec<u64> = Vec::new();
    let mut needed_offsets: Vec<u64> = Vec::new();
    for index in 0..(dyn_size / entry).min(MAX_DYNAMIC_ENTRIES) {
        let at = dyn_offset.checked_add(index * entry)?;
        let tag = reader.word(at)?;
        let value = reader.word(at + word)?;
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed_offsets.push(value),
            DT_STRTAB => strtab_vaddr = Some(value),
            DT_RPATH | DT_RUNPATH => rpath_offsets.push(value),
            _ => {}
        }
    }
    let strtab_vaddr = strtab_vaddr?;
    let strtab_file = loads.iter().find_map(|(vaddr, offset, filesz)| {
        let delta = strtab_vaddr.checked_sub(*vaddr)?;
        (delta < *filesz)
            .then(|| offset.checked_add(delta))
            .flatten()
    })?;

    let read_string = |offset: u64| -> Option<String> {
        let start = usize::try_from(strtab_file.checked_add(offset)?).ok()?;
        let tail = bytes.get(start..)?;
        let end = tail
            .iter()
            .take(MAX_RPATH_BYTES)
            .position(|byte| *byte == 0)
            .unwrap_or_else(|| tail.len().min(MAX_RPATH_BYTES));
        Some(String::from_utf8_lossy(&tail[..end]).into_owned())
    };
    let mut result = DynamicStrings::default();
    for offset in needed_offsets {
        result.needed.push(read_string(offset)?);
    }
    for offset in rpath_offsets {
        result.rpaths.push(read_string(offset)?);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(out: &mut Vec<u8>, value: u64, size: usize, big_endian: bool) {
        let bytes = value.to_le_bytes();
        if big_endian {
            out.extend(bytes[..size].iter().rev());
        } else {
            out.extend(&bytes[..size]);
        }
    }

    /// A minimal ELF with one `PT_LOAD` covering the file and a `PT_DYNAMIC`
    /// holding `DT_STRTAB`, the given rpath-class tag, and `DT_NULL`.
    fn synthetic_elf(is64: bool, big_endian: bool, rpath_tag: u64, rpath: &str) -> Vec<u8> {
        synthetic_elf_with_needed(is64, big_endian, rpath_tag, rpath, None)
    }

    /// As [`synthetic_elf`], plus a `DT_NEEDED` entry naming `needed`. The
    /// needed name shares the string table: `<rpath>\0<needed>\0`.
    fn synthetic_elf_with_needed(
        is64: bool,
        big_endian: bool,
        rpath_tag: u64,
        rpath: &str,
        needed: Option<&str>,
    ) -> Vec<u8> {
        let word = if is64 { 8 } else { 4 };
        let (ehsize, phentsize) = if is64 { (64usize, 56usize) } else { (52, 32) };
        let phnum = 2;
        let dyn_offset = ehsize + phentsize * phnum;
        let dyn_entries = if needed.is_some() { 4 } else { 3 };
        let dyn_size = word * 2 * dyn_entries;
        let strtab_offset = dyn_offset + dyn_size;
        let base_vaddr = 0x40_0000u64;

        let mut out = Vec::new();
        out.extend(b"\x7fELF");
        out.push(if is64 { 2 } else { 1 });
        out.push(if big_endian { 2 } else { 1 });
        out.push(1);
        out.resize(16, 0);
        put(&mut out, 3, 2, big_endian); // e_type
        put(&mut out, 62, 2, big_endian); // e_machine
        put(&mut out, 1, 4, big_endian); // e_version
        put(&mut out, 0, word, big_endian); // e_entry
        put(&mut out, ehsize as u64, word, big_endian); // e_phoff
        put(&mut out, 0, word, big_endian); // e_shoff
        put(&mut out, 0, 4, big_endian); // e_flags
        put(&mut out, ehsize as u64, 2, big_endian);
        put(&mut out, phentsize as u64, 2, big_endian);
        put(&mut out, phnum as u64, 2, big_endian);
        put(&mut out, 0, 6, big_endian); // shentsize, shnum, shstrndx
        assert_eq!(out.len(), ehsize);

        let needed_offset = (rpath.len() + 1) as u64;
        let strings_len = rpath.len() + 1 + needed.map_or(0, |name| name.len() + 1);
        let total = (strtab_offset + strings_len) as u64;
        for (kind, offset, vaddr, filesz) in [
            (PT_LOAD, 0u64, base_vaddr, total),
            (
                PT_DYNAMIC,
                dyn_offset as u64,
                base_vaddr + dyn_offset as u64,
                dyn_size as u64,
            ),
        ] {
            put(&mut out, u64::from(kind), 4, big_endian);
            if is64 {
                put(&mut out, 5, 4, big_endian); // flags
                for field in [offset, vaddr, vaddr, filesz, filesz, 8] {
                    put(&mut out, field, 8, big_endian);
                }
            } else {
                for field in [offset, vaddr, vaddr, filesz, filesz, 5, 4] {
                    put(&mut out, field, 4, big_endian);
                }
            }
        }
        let mut entries = vec![
            (DT_STRTAB, base_vaddr + strtab_offset as u64),
            (rpath_tag, 0),
        ];
        if needed.is_some() {
            entries.push((DT_NEEDED, needed_offset));
        }
        entries.push((DT_NULL, 0));
        for (tag, value) in entries {
            put(&mut out, tag, word, big_endian);
            put(&mut out, value, word, big_endian);
        }
        assert_eq!(out.len(), strtab_offset);
        out.extend(rpath.as_bytes());
        out.push(0);
        if let Some(name) = needed {
            out.extend(name.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn origin_in_runpath_or_rpath_is_position_dependent_in_every_layout() {
        for (is64, big_endian) in [(true, false), (true, true), (false, false), (false, true)] {
            for tag in [DT_RUNPATH, DT_RPATH] {
                for text in ["$ORIGIN/../soldr.libs", "/opt/x:${ORIGIN}/lib"] {
                    let elf = synthetic_elf(is64, big_endian, tag, text);
                    assert!(
                        elf_has_origin_rpath(&elf),
                        "is64={is64} be={big_endian} tag={tag} {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn absolute_rpaths_and_data_mentions_are_not_position_dependent() {
        let absolute = synthetic_elf(true, false, DT_RUNPATH, "/usr/lib:/opt/lib");
        assert!(!elf_has_origin_rpath(&absolute));

        // `$ORIGIN` present in the file, but only as trailing data, not in an rpath.
        let mut in_data = synthetic_elf(true, false, DT_RUNPATH, "/usr/lib");
        in_data.extend(b"\0$ORIGIN/../not-an-rpath\0");
        assert!(!elf_has_origin_rpath(&in_data));
    }

    #[test]
    fn unparseable_input_answers_false_without_panicking() {
        assert!(!elf_has_origin_rpath(b""));
        assert!(!elf_has_origin_rpath(b"#!/bin/sh\necho $ORIGIN\n"));
        assert!(!elf_has_origin_rpath(b"\x7fELF\x02\x01\x01\x00 $ORIGIN"));
        let full = synthetic_elf(true, false, DT_RUNPATH, "$ORIGIN/x");
        for cut in 0..full.len() {
            let _ = elf_has_origin_rpath(&full[..cut]);
        }
        let mut corrupt = full.clone();
        corrupt[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(!elf_has_origin_rpath(&corrupt));
    }

    #[test]
    fn needed_libraries_are_listed_in_every_layout() {
        for (is64, big_endian) in [(true, false), (true, true), (false, false), (false, true)] {
            let elf = synthetic_elf_with_needed(
                is64,
                big_endian,
                DT_RUNPATH,
                "/usr/lib",
                Some("liblzma.so.5"),
            );
            assert_eq!(
                elf_needed_libraries(&elf),
                Some(vec!["liblzma.so.5".to_string()]),
                "is64={is64} be={big_endian}"
            );
            let without = synthetic_elf(is64, big_endian, DT_RUNPATH, "/usr/lib");
            assert_eq!(elf_needed_libraries(&without), Some(Vec::new()));
        }
        assert_eq!(elf_needed_libraries(b"#!/bin/sh\n"), None);
    }
}

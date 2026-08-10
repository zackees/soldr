//! Turn raw captures into return addresses (#635).
//!
//! # This runs after every thread is resumed
//!
//! Unwinding never touches a live thread. It reads only the register values
//! and stack bytes copied during the suspend window, so it can allocate,
//! take locks, and take as long as it needs — the target is already running.
//! That separation is the reason the capture path can stay as small as it is.
//!
//! The mechanism is `read_stack`: framehop asks for a `u64` at an address, and
//! we answer out of the copied slice at `addr - stack_pointer`. Addresses
//! outside the copied window return `Err`, which framehop treats as the end of
//! what it can walk. A truncated capture therefore yields a shorter stack
//! rather than a wrong one.
//!
//! # Addresses, not symbols
//!
//! The output is return addresses. Resolving them to function names is
//! symbolization, which happens off-process in a later slice.

use std::ops::Range;

#[cfg(windows)]
use framehop::ModuleSectionInfo;
use framehop::{Module, Unwinder};

#[cfg(target_arch = "aarch64")]
use framehop::aarch64::{
    CacheAarch64 as ArchCache, UnwindRegsAarch64 as ArchRegs, UnwinderAarch64 as ArchUnwinder,
};
#[cfg(target_arch = "x86_64")]
use framehop::x86_64::{
    CacheX86_64 as ArchCache, UnwindRegsX86_64 as ArchRegs, UnwinderX86_64 as ArchUnwinder,
};

/// Build the architecture's register set from a captured sample.
///
/// The two constructors take different triples, and the difference is easy to
/// get wrong silently: x86_64 wants the instruction pointer, aarch64 wants the
/// **link register**. A leaf frame's return address lives in LR on aarch64, so
/// passing the PC there yields a plausible-looking but wrong innermost frame.
#[cfg(target_arch = "x86_64")]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    ArchRegs::new(
        sample.instruction_pointer,
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    ArchRegs::new(
        // Falls back to the PC only if the capture had no LR, which would
        // itself be a capture bug rather than a normal state.
        sample.link_register.unwrap_or(sample.instruction_pointer),
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    let mask = framehop::aarch64::PtrAuthMask::new_24_40();
    ArchRegs::new_with_ptr_auth_mask(
        mask,
        sample.link_register.unwrap_or(sample.instruction_pointer),
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn instruction_pointer(sample: &ThreadSample) -> u64 {
    framehop::aarch64::PtrAuthMask::new_24_40().strip_ptr_auth(sample.instruction_pointer)
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
fn instruction_pointer(sample: &ThreadSample) -> u64 {
    sample.instruction_pointer
}

use super::modules::LoadedModule;
use super::{Snapshot, ThreadSample};

/// Sections framehop may ask for. `.pdata` plus `.rdata`/`.xdata` carry the
/// x86_64 unwind tables; `.text` anchors code addresses.
#[cfg(windows)]
const WANTED_SECTIONS: &[&str] = &[".text", ".pdata", ".rdata", ".xdata"];

/// Adapts a [`LoadedModule`] to framehop's section interface.
///
/// Section bytes are read straight from the mapped image — the module is in
/// our own address space, so this is a slice, not file I/O.
#[cfg(windows)]
struct MappedModuleSections {
    base: u64,
    sections: Vec<(String, Range<u64>)>,
}

#[cfg(windows)]
impl MappedModuleSections {
    fn new(module: &LoadedModule) -> Self {
        Self {
            base: module.base,
            sections: module
                .sections
                .iter()
                .filter(|s| WANTED_SECTIONS.contains(&s.name.as_str()))
                .map(|s| (s.name.clone(), s.range.clone()))
                .collect(),
        }
    }

    fn find(&self, name: &[u8]) -> Option<Range<u64>> {
        let name = std::str::from_utf8(name).ok()?;
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.clone())
    }
}

#[cfg(windows)]
impl ModuleSectionInfo<Vec<u8>> for MappedModuleSections {
    fn base_svma(&self) -> u64 {
        // For PE this is the image base, and because we read the *mapped*
        // image its stated and actual bases coincide.
        self.base
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<Range<u64>> {
        self.find(name)
    }

    fn section_data(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let range = self.find(name)?;
        let len = usize::try_from(range.end.saturating_sub(range.start)).ok()?;
        if len == 0 {
            return None;
        }
        // Safety: the range came from the mapped module's own section table,
        // so it is committed memory inside this process for as long as the
        // module stays loaded.
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(range.start as *const u8, len) }.to_vec();
        Some(bytes)
    }
}

/// Build an unwinder covering `modules`.
#[cfg(windows)]
pub fn build_unwinder(modules: &[LoadedModule]) -> ArchUnwinder<Vec<u8>> {
    let mut unwinder = ArchUnwinder::new();
    for module in modules {
        let range = module.range();
        unwinder.add_module(Module::new(
            format!("{:#x}", module.base),
            range.clone(),
            module.base,
            MappedModuleSections::new(module),
        ));
    }
    unwinder
}

/// Unwind one captured thread into return addresses.
///
/// Reads only `sample`'s copied bytes; the thread itself is long since
/// resumed.
pub fn unwind_sample(
    unwinder: &ArchUnwinder<Vec<u8>>,
    cache: &mut ArchCache,
    sample: &ThreadSample,
    _modules: &[LoadedModule],
) -> Vec<u64> {
    let sp = sample.stack_pointer;
    let bytes = &sample.stack_bytes;

    // Answer stack reads out of the copy. Anything outside the captured window
    // is Err, which ends the walk — a truncated capture yields a shorter
    // stack, never a fabricated one.
    let mut read_stack = |addr: u64| -> Result<u64, ()> {
        let offset = addr.checked_sub(sp).ok_or(())?;
        let offset = usize::try_from(offset).map_err(|_| ())?;
        let end = offset.checked_add(8).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[offset..end]);
        Ok(u64::from_le_bytes(word))
    };

    let regs = arch_regs(sample);

    let mut frames = Vec::new();
    let mut iter = unwinder.iter_frames(instruction_pointer(sample), regs, cache, &mut read_stack);
    while let Ok(Some(frame)) = iter.next() {
        frames.push(frame.address());
    }

    // Coverage instrumentation and hand-written assembly can expose valid
    // frame-pointer chains that are not described by usable ELF/Mach-O CFI.
    // Recover those callers conservatively from the already-copied stack.
    // Windows deliberately remains metadata-only so its PE unwind-table path
    // is still exercised by the known-stack acceptance test.
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    if let Some(suspect_from) = frames
        .iter()
        .skip(1)
        .position(|address| {
            !_modules
                .iter()
                .any(|module| module.contains_executable(*address))
        })
        .map(|index| index + 1)
        .or((frames.len() <= 1).then_some(frames.len()))
    {
        let recovered = frame_pointer_fallback(sample, |address| {
            _modules
                .iter()
                .any(|module| module.contains_executable(address))
        });
        replace_suspect_callers(&mut frames, suspect_from, recovered);
    }

    frames
}

/// Replace an invalid metadata-derived caller tail with a validated chain.
#[cfg(all(
    target_arch = "x86_64",
    any(test, target_os = "linux", target_os = "macos")
))]
fn replace_suspect_callers(frames: &mut Vec<u64>, suspect_from: usize, recovered: Vec<u64>) {
    let valid_prefix_len = suspect_from.min(frames.len());
    let valid_callers = valid_prefix_len.saturating_sub(1);
    let overlap = if valid_callers == 0 {
        0
    } else {
        (1..=valid_callers.min(recovered.len()))
            .rev()
            .find(|&len| frames[valid_prefix_len - len..valid_prefix_len] == recovered[..len])
            .unwrap_or(0)
    };

    // Always discard the known-invalid metadata tail. With no metadata caller
    // prefix, the independently validated chain can be used in full. Otherwise
    // extend only when the two unwind methods overlap, preserving frameless
    // metadata-derived callers that are absent from an RBP chain.
    frames.truncate(valid_prefix_len);
    if valid_callers == 0 || overlap != 0 {
        frames.extend_from_slice(&recovered[overlap..]);
    }
}

/// Walk a conventional x86_64 RBP chain inside the bounded stack copy.
///
/// Every read is range-checked. Alignment and strictly increasing frame
/// pointers reject arbitrary general-purpose RBP values, every return address
/// must satisfy `is_return_address`, and the depth cap makes corrupt cyclic
/// chains terminate.
#[cfg(all(
    target_arch = "x86_64",
    any(test, target_os = "linux", target_os = "macos")
))]
fn frame_pointer_fallback(
    sample: &ThreadSample,
    mut is_return_address: impl FnMut(u64) -> bool,
) -> Vec<u64> {
    const MAX_FRAMES: usize = 256;

    let stack_start = sample.stack_pointer;
    let Some(stack_end) = stack_start.checked_add(sample.stack_bytes.len() as u64) else {
        return Vec::new();
    };
    let mut frame_pointer = sample.frame_pointer;
    let mut frames = Vec::new();

    let read_word = |address: u64| -> Option<u64> {
        if address % 8 != 0 {
            return None;
        }
        let offset = usize::try_from(address.checked_sub(stack_start)?).ok()?;
        let end = offset.checked_add(8)?;
        let bytes = sample.stack_bytes.get(offset..end)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    };

    for _ in 0..MAX_FRAMES {
        if frame_pointer < stack_start
            || frame_pointer
                .checked_add(16)
                .is_none_or(|end| end > stack_end)
        {
            break;
        }
        let Some(previous) = read_word(frame_pointer) else {
            break;
        };
        let Some(return_address) = read_word(frame_pointer + 8) else {
            break;
        };
        if return_address == 0 || !is_return_address(return_address) {
            break;
        }
        frames.push(return_address);
        if previous <= frame_pointer {
            break;
        }
        frame_pointer = previous;
    }

    frames
}

#[cfg(all(test, target_arch = "x86_64"))]
mod frame_pointer_tests {
    use super::*;
    use crate::snapshot::CaptureKind;

    fn sample(frame_pointer: u64, stack_bytes: Vec<u8>) -> ThreadSample {
        ThreadSample {
            os_tid: 1,
            stack_pointer: 0x1000,
            instruction_pointer: 0x2000,
            frame_pointer,
            link_register: None,
            stack_bytes,
            truncated: false,
            kind: CaptureKind::RawContext,
            frames: Vec::new(),
        }
    }

    fn write_word(stack: &mut [u8], address: u64, value: u64) {
        let offset = usize::try_from(address - 0x1000).unwrap();
        stack[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn frame_pointer_fallback_walks_a_bounded_monotonic_chain() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1020);
        write_word(&mut stack, 0x1008, 0xaaaa);
        write_word(&mut stack, 0x1020, 0);
        write_word(&mut stack, 0x1028, 0xbbbb);

        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |_| true),
            vec![0xaaaa, 0xbbbb]
        );
    }

    #[test]
    fn frame_pointer_fallback_rejects_unaligned_or_backward_chains() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1000);
        write_word(&mut stack, 0x1008, 0xaaaa);

        assert!(frame_pointer_fallback(&sample(0x1001, stack.clone()), |_| true).is_empty());
        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |_| true),
            vec![0xaaaa]
        );
    }

    #[test]
    fn frame_pointer_fallback_stops_at_an_unattributed_return_address() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1020);
        write_word(&mut stack, 0x1008, 0xaaaa);
        write_word(&mut stack, 0x1020, 0);
        write_word(&mut stack, 0x1028, 0xbbbb);

        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |address| address == 0xaaaa),
            vec![0xaaaa]
        );
    }

    #[test]
    fn recovered_chain_replaces_the_suspect_framehop_tail() {
        let mut frames = vec![0x2000, 0xdead];

        replace_suspect_callers(&mut frames, 1, vec![0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0xaaaa, 0xbbbb]);
    }

    #[test]
    fn recovered_chain_preserves_duplicates_and_valid_framehop_prefix() {
        let mut frames = vec![0x2000, 0xaaaa, 0xdead];

        replace_suspect_callers(&mut frames, 2, vec![0xaaaa, 0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0xaaaa, 0xaaaa, 0xbbbb]);
    }

    #[test]
    fn recovered_chain_without_overlap_keeps_only_the_valid_prefix() {
        let mut frames = vec![0x2000, 0x1111, 0xdead];

        replace_suspect_callers(&mut frames, 2, vec![0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0x1111]);
    }
}

/// Unwind every sample in `snapshot`, recording the result.
///
/// Sets `frames_resolved` only here — the one place frames actually exist.
#[cfg(windows)]
pub fn resolve_frames(snapshot: &mut Snapshot, modules: &[LoadedModule]) {
    let unwinder = build_unwinder(modules);
    let mut cache = ArchCache::new();

    for sample in &mut snapshot.threads {
        sample.frames = unwind_sample(&unwinder, &mut cache, sample, modules);
    }
    snapshot.frames_resolved = true;
}

/// Add Linux ELF or macOS Mach-O unwind metadata from the modules currently
/// mapped in this process. Object parsing happens only after capture, when
/// every sibling is running again.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_unix_unwinder(modules: &[LoadedModule]) -> ArchUnwinder<Vec<u8>> {
    use framehop::ExplicitModuleSectionInfo;
    #[cfg(target_os = "macos")]
    use object::ObjectSegment;
    use object::{Object, ObjectSection};

    fn section(file: &object::File<'_>, names: &[&str]) -> (Option<Range<u64>>, Option<Vec<u8>>) {
        for section in file.sections() {
            let Ok(name) = section.name() else {
                continue;
            };
            if !names.contains(&name) {
                continue;
            }
            let range = section.address()..section.address().saturating_add(section.size());
            let data = section
                .uncompressed_data()
                .ok()
                .map(|data| data.into_owned());
            return (Some(range), data);
        }
        (None, None)
    }

    let mut unwinder = ArchUnwinder::new();
    for module in modules {
        let Some(path) = module.path.as_deref() else {
            continue;
        };
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };

        let (text_svma, text) = section(&file, &[".text", "__text"]);
        let (eh_frame_svma, eh_frame) = section(&file, &[".eh_frame", "__eh_frame"]);
        let (eh_frame_hdr_svma, eh_frame_hdr) =
            section(&file, &[".eh_frame_hdr", "__eh_frame_hdr"]);
        let (_, unwind_info) = section(&file, &["__unwind_info"]);
        let (stubs_svma, _) = section(&file, &["__stubs"]);
        let (stub_helper_svma, _) = section(&file, &["__stub_helper"]);
        let (got_svma, _) = section(&file, &[".got", "__got"]);

        #[cfg(target_os = "linux")]
        let base_svma = 0;
        #[cfg(target_os = "macos")]
        let base_svma = file.relative_address_base();

        #[cfg(target_os = "macos")]
        let mut text_segment_svma = None;
        #[cfg(not(target_os = "macos"))]
        let text_segment_svma = None;
        #[cfg(target_os = "macos")]
        let mut text_segment = None;
        #[cfg(not(target_os = "macos"))]
        let text_segment = None;
        #[cfg(target_os = "macos")]
        for segment in file.segments() {
            if segment.name().ok().flatten() == Some("__TEXT") {
                text_segment_svma =
                    Some(segment.address()..segment.address().saturating_add(segment.size()));
                text_segment = segment.data().ok().map(ToOwned::to_owned);
                break;
            }
        }

        let info = ExplicitModuleSectionInfo {
            base_svma,
            text_svma,
            text,
            stubs_svma,
            stub_helper_svma,
            got_svma,
            unwind_info,
            eh_frame_svma,
            eh_frame,
            eh_frame_hdr_svma,
            eh_frame_hdr,
            text_segment_svma,
            text_segment,
            ..Default::default()
        };
        unwinder.add_module(Module::new(
            path.to_owned(),
            module.range(),
            module.base,
            info,
        ));
    }
    unwinder
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Resolve every raw sample against the current process's ELF/Mach-O images.
pub fn resolve_frames_for_current_process(snapshot: &mut Snapshot) -> std::io::Result<()> {
    let modules = super::modules::enumerate_modules()?;
    let unwinder = build_unix_unwinder(&modules);
    let mut cache = ArchCache::new();
    for sample in &mut snapshot.threads {
        sample.frames = unwind_sample(&unwinder, &mut cache, sample, &modules);
    }
    snapshot.frames_resolved = true;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::modules::{enumerate_modules, module_for_address};
    use super::super::{capture_all_threads, SnapshotConfig};
    use super::*;

    #[inline(never)]
    fn inner_frame(flag: &std::sync::atomic::AtomicBool) {
        // Spin briefly so the sibling capture observes this frame on the stack.
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn unwinder_covers_every_enumerated_module() {
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        // Construction must not panic and must accept every module we found.
        // (framehop has no public module count, so this asserts the build path
        // is total rather than a specific number.)
        let _ = unwinder;
        assert!(!modules.is_empty());
    }

    #[test]
    fn reads_outside_the_captured_window_end_the_walk() {
        let sample = ThreadSample {
            os_tid: 1,
            stack_pointer: 0x1000,
            instruction_pointer: 0x2000,
            frame_pointer: 0,
            link_register: None,
            // Deliberately tiny: any read past 16 bytes must fail rather than
            // read adjacent memory.
            stack_bytes: vec![0u8; 16],
            truncated: true,
            kind: super::super::CaptureKind::RawContext,
            frames: Vec::new(),
        };
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        let mut cache = ArchCache::new();

        // Must terminate, not panic or spin, on a stack it cannot follow.
        let frames = unwind_sample(&unwinder, &mut cache, &sample, &modules);
        assert!(
            frames.len() <= 2,
            "a 16-byte stack cannot yield a deep walk, got {}",
            frames.len()
        );
    }

    /// The real check: unwind a live capture and confirm at least one returned
    /// address lands inside a module's `.text`.
    ///
    /// The oracle is the independently-verified module inventory from #700 —
    /// the unwinder is not allowed to confirm itself.
    #[test]
    fn unwound_addresses_fall_inside_known_text_sections() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || inner_frame(&stop))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut snapshot = capture_all_threads(&SnapshotConfig::default()).expect("capture");
        let modules = enumerate_modules().expect("modules");
        resolve_frames(&mut snapshot, &modules);

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        assert!(snapshot.frames_resolved, "resolve_frames must mark this");

        let mut in_text = 0usize;
        for sample in &snapshot.threads {
            for &addr in &sample.frames {
                if let Some(m) = module_for_address(&modules, addr) {
                    if m.section(".text").is_some_and(|t| t.range.contains(&addr)) {
                        in_text += 1;
                    }
                }
            }
        }

        assert!(
            in_text > 0,
            "no unwound address landed in any module's .text; frames were {:?}",
            snapshot
                .threads
                .iter()
                .map(|s| s.frames.len())
                .collect::<Vec<_>>()
        );
    }
}

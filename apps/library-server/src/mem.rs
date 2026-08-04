//! The host's view of this process's own memory.
//!
//! Kept in sync with `apps/library-app/src/mem.rs`, which is the same file
//! for the other host. Two copies beat a two-function crate, and beat putting
//! Apple calls in `library-core`, which is deliberately platform-free.
//!
//! `memory_stats` gives us mach `resident_size`, which counts only pages
//! currently backed by physical RAM. macOS compresses an idle process's heap,
//! so resident_size can fall to a small fraction of what the process is
//! actually holding — the perf panel once showed 23 MiB against 205 MiB of
//! accounted line items. `phys_footprint` is the number that survives that:
//! it counts compressed and swapped pages, it is what Activity Monitor's
//! "Memory" column reports, and it is what jetsam and memory pressure judge.

use library_core::perf::HostMem;

/// Both numbers, for [`library_core::perf::memory`] to reconcile against.
pub(crate) fn host_mem() -> HostMem {
    HostMem {
        rss_bytes: memory_stats::memory_stats().map(|m| m.physical_mem as u64),
        footprint_bytes: phys_footprint(),
    }
}

/// `TASK_VM_INFO.phys_footprint`, or `None` if the kernel wouldn't say.
#[cfg(target_os = "macos")]
fn phys_footprint() -> Option<u64> {
    vm_info().map(|i| i.phys_footprint)
}

/// `struct task_vm_info` has grown across releases — the header marks
/// phys_footprint "added for rev1" — and the count-in/count-out protocol is
/// how a caller says which revision it was built against: the kernel fills
/// min(our count, its own) words and reports back how many it wrote. So we
/// declare exactly the prefix through phys_footprint and ask for that much. A
/// kernel too old to have the field answers with a shorter count, which the
/// check below turns into `None` rather than a silently wrong number.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct TaskVmInfoPrefix {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

#[cfg(target_os = "macos")]
fn vm_info() -> Option<TaskVmInfoPrefix> {
    // the struct's size in natural_t (u32) words — the unit task_info counts in
    const PREFIX_COUNT: mach2::message::mach_msg_type_number_t = (size_of::<TaskVmInfoPrefix>()
        / size_of::<u32>())
        as mach2::message::mach_msg_type_number_t;

    let mut info = TaskVmInfoPrefix::default();
    let mut count = PREFIX_COUNT;
    // SAFETY: `info` is a live, correctly-aligned #[repr(C)] buffer of exactly
    // `count` natural_t words, which is what task_info writes into; the call
    // borrows nothing past its return.
    let kr = unsafe {
        mach2::task::task_info(
            mach2::traps::mach_task_self(),
            mach2::task_info::TASK_VM_INFO,
            std::ptr::addr_of_mut!(info).cast::<mach2::vm_types::integer_t>(),
            &mut count,
        )
    };
    // A short count means the kernel stopped before phys_footprint; anything
    // it did write about the earlier fields is not what we asked for.
    if kr != mach2::kern_return::KERN_SUCCESS || count < PREFIX_COUNT {
        return None;
    }
    Some(info)
}

#[cfg(not(target_os = "macos"))]
fn phys_footprint() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The one way this can fail silently is a wrong field offset, which would
    // return some neighbouring field's value. So pin the layout against a
    // number we can get independently: `memory_stats` reads resident_size via
    // MACH_TASK_BASIC_INFO, an entirely different flavor with its own struct.
    // If our TASK_VM_INFO prefix agrees with it on resident_size, every offset
    // before it is right — and phys_footprint sits immediately after the run
    // this checks the far end of.
    #[test]
    #[cfg(target_os = "macos")]
    fn struct_layout_agrees_with_an_independent_resident_size() {
        let info = vm_info().expect("macOS should answer TASK_VM_INFO");
        let independent = memory_stats::memory_stats()
            .map(|m| m.physical_mem as u64)
            .expect("MACH_TASK_BASIC_INFO should answer");

        // Both are sampled a few instructions apart on a live process, so
        // allow drift — but a misread field is off by orders of magnitude,
        // not by a few pages.
        let (lo, hi) = (independent / 2, independent * 2);
        let got = info.resident_size;
        assert!(
            got >= lo && got <= hi,
            "TASK_VM_INFO resident_size {got} disagrees with MACH_TASK_BASIC_INFO {independent} \
             — the struct prefix is probably misaligned, which would also corrupt phys_footprint"
        );
        // 16K on Apple silicon, 4K on Intel — anything else means we read a
        // field that isn't page_size.
        assert!(
            matches!(info.page_size, 4096 | 16384),
            "page_size {} is not a page size",
            info.page_size
        );
    }

    // Bounds loose enough not to flake, tight enough that a pointer-valued
    // field (what a too-long prefix would land on) trips one of them.
    #[test]
    #[cfg(target_os = "macos")]
    fn phys_footprint_is_plausible_for_this_process() {
        let f = phys_footprint().expect("macOS should answer TASK_VM_INFO");
        assert!(f > 1 << 20, "footprint {f} is below 1 MiB");
        assert!(f < 64 << 30, "footprint {f} is above 64 GiB");
    }

    // The whole point: footprint counts what resident_size drops. It may sit
    // below rss transiently, but not by the order of magnitude that would
    // mean we read the wrong field.
    #[test]
    #[cfg(target_os = "macos")]
    fn footprint_is_the_same_order_as_rss() {
        let h = host_mem();
        let (rss, f) = (
            h.rss_bytes.expect("rss probe"),
            h.footprint_bytes.expect("footprint probe"),
        );
        assert!(f >= rss / 4, "footprint {f} is implausibly below rss {rss}");
    }

    #[test]
    fn host_mem_prefers_footprint_when_both_land() {
        let h = host_mem();
        if h.footprint_bytes.is_some() {
            assert_eq!(h.total(), h.footprint_bytes);
        }
    }
}

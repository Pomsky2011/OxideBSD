//! Enables SSE for the whole system: `CR0`/`CR4` bits that were never touched before this, since
//! nothing needed them.
//!
//! Discovered as a real, previously-latent gap while bringing up `musl-smoke` (see `CLAUDE.md`'s
//! musl section) — this kernel's own build target (`x86_64-oxidebsd.json`) disables SSE/MMX in
//! its own codegen (`disable-redzone: true`, `+soft-float`), and every userland crate written
//! *for* that target (`ring3-smoke`, `stsh`, `fork-exec-smoke`) inherits the same restriction, so
//! none of them ever emit an SSE instruction. A real musl static binary, built with an ordinary
//! host `gcc` targeting plain x86_64 (SSE2 baseline, per the standard ABI), is the first userland
//! binary this kernel has ever run that isn't built against `x86_64-oxidebsd.json` — and it
//! `#UD`'d on its very first `pxor` (inside musl's own stdio buffer init). Per the SDM: executing
//! an SSE instruction while `CR4.OSFXSR` is clear raises `#UD`, not a more suggestive fault — this
//! kernel never sets that bit (or `CR0.EM`/`CR0.MP`) anywhere, so it was purely accidental that
//! nothing surfaced this until now.
//!
//! Deliberately not "lazy" FPU state switching (`CR0.TS` + `#NM`-triggered save/restore, the
//! classic optimization real kernels use to skip saving FPU/SSE state for threads that never touch
//! it): this kernel's context switch (`context_switch::switch_context`) doesn't save/restore
//! `XMM`/`x87` state at all, lazily or otherwise. That's fine as long as at most one process is
//! ever actually *using* SSE at a time without yielding mid-computation — true for `musl-smoke`
//! today (a single process, no preemption) but a real gap the moment two SSE-using processes
//! could interleave. Flagged here, not fixed: fixing it means extending `SwitchFrame`/
//! `switch_context` to `fxsave`/`fxrstor`, a real (if mechanical) follow-up, not attempted yet.
pub fn init() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

    // SAFETY: clearing EMULATE_COPROCESSOR and setting MONITOR_COPROCESSOR/OSFXSR/
    // OSXMMEXCPT_ENABLE is exactly the documented "enable SSE" sequence (see the OSDev wiki's SSE
    // page, or the SDM's discussion of CR0.EM/CR4.OSFXSR) -- safe unconditionally this early in
    // boot, before anything (kernel or user) has executed an SSE instruction that could be
    // affected by the transition.
    unsafe {
        Cr0::write((Cr0::read() & !Cr0Flags::EMULATE_COPROCESSOR) | Cr0Flags::MONITOR_COPROCESSOR);
        Cr4::write(Cr4::read() | Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE);
    }
}

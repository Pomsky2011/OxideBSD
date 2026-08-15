//! A real, cryptographically-mixed random byte source backing `/dev/random`/`/dev/urandom`
//! (`modules/oxfs/`'s synthetic `/dev` -- see that module's own `dev_open`). Every call gathers
//! several independent, hard-for-an-outside-observer-to-predict values (the CPU cycle counter,
//! PIT tick count, RTC wall-clock, a monotonic call counter, a stack address, and real hardware
//! `RDRAND` output when the CPU supports it), hashes them all together with SHA-256 (RustCrypto's
//! `sha2` crate) into a 32-byte key, then generates the caller's requested output as a ChaCha20
//! (RustCrypto's `chacha20` crate) keystream under that key -- the same "gather a pile of diverse
//! state, hash it into a seed, drive a real algorithm with it" design Pokemon Black/White's own
//! famous RNG made popular, just with a real stream cipher standing in for that game's own LCG.
//!
//! **No persistent DRBG state to reseed on a schedule** -- unlike real Linux's `/dev/urandom`
//! (a long-lived pool, periodically reseeded), every call here gathers fresh entropy and derives
//! an independent key from scratch, so there's no reseed-interval policy to get wrong. A fixed,
//! all-zero ChaCha20 nonce is safe specifically *because* of this: the algorithm's real security
//! requirement is "never reuse a `(key, nonce)` pair," and a freshly derived key is never reused
//! across two different calls.
//!
//! **Crypto primitives are vetted, external crates (`sha2`/`chacha20`), not hand-rolled** --
//! deliberately different from this codebase's usual "own the small stuff" bias (`src/pic.rs`
//! instead of `pic8259`, the hand-written syscall ABI, ...): a subtle bug in a hand-written hash
//! or stream cipher is far harder to catch than one in a driver or allocator, and this is exactly
//! the class of code where "well-audited, widely used implementation" outweighs "we wrote it
//! ourselves," the same reasoning `linked_list_allocator`/the `x86_64` crate already followed.
//!
//! Not a real Linux-style entropy pool (no blocking-until-enough-entropy distinction) -- both
//! `/dev/random` and `/dev/urandom` share this exact same source, matching modern Linux's own
//! post-5.6 stance that the two barely differ once a pool is initialized.

use core::arch::x86_64::{__cpuid, _rdtsc};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha256};

static CALL_COUNTER: AtomicU64 = AtomicU64::new(0);
static RDRAND_CHECKED: AtomicBool = AtomicBool::new(false);
static RDRAND_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Checked once (via `CPUID` leaf `1`, `ECX` bit `30`) and cached -- real hardware entropy is a
/// bonus input when present, not a hard requirement (QEMU/TCG without KVM, or an old host CPU,
/// simply won't have it; `gather_seed` below still has plenty of other jittery sources).
fn rdrand_available() -> bool {
    if !RDRAND_CHECKED.load(Ordering::Relaxed) {
        // CPUID leaf 1 is always a valid, side-effect-free query on x86_64 -- safe in this Rust
        // version, no unsafe block needed.
        let regs = __cpuid(1);
        RDRAND_AVAILABLE.store(regs.ecx & (1 << 30) != 0, Ordering::Relaxed);
        RDRAND_CHECKED.store(true, Ordering::Relaxed);
    }
    RDRAND_AVAILABLE.load(Ordering::Relaxed)
}

/// Only ever called after `rdrand_available()` has confirmed `CPUID` reports the instruction
/// supported -- `#[target_feature]` (rather than a blanket `-C target-feature=+rdrand` for the
/// whole kernel) keeps this the one function allowed to emit the instruction. Intel's own
/// documented retry protocol: a handful of attempts is more than enough in practice.
#[target_feature(enable = "rdrand")]
unsafe fn rdrand64() -> Option<u64> {
    let mut val: u64 = 0;
    for _ in 0..10 {
        // _rdrand64_step is safe in this Rust version -- this function's own `unsafe` comes
        // entirely from the `#[target_feature(enable = "rdrand")]` above, not from this call.
        if core::arch::x86_64::_rdrand64_step(&mut val) == 1 {
            return Some(val);
        }
        core::hint::spin_loop();
    }
    None
}

/// Gathers this call's own pile of entropy sources and hashes them into a 32-byte SHA-256 digest
/// -- see this module's own doc comment for the full design.
fn gather_seed() -> [u8; 32] {
    let mut hasher = Sha256::new();

    // SAFETY: RDTSC is a plain, always-available x86_64 instruction (no CPUID gate needed, unlike
    // RDRAND below) -- just reads the CPU's free-running cycle counter.
    let tsc = unsafe { _rdtsc() };
    hasher.update(tsc.to_le_bytes());

    hasher.update(crate::cpu::interrupts::ticks().to_le_bytes());
    hasher.update((crate::cpu::rtc::unix_epoch_seconds() as u64).to_le_bytes());
    hasher.update(CALL_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());

    // A stack address -- varies with call depth/whatever else has touched this kernel stack
    // recently, a weak but free extra source alongside the stronger ones above.
    let stack_marker: u8 = 0;
    hasher.update((&raw const stack_marker as u64).to_le_bytes());

    if rdrand_available() {
        // SAFETY: rdrand_available() just confirmed CPUID reports this instruction supported.
        if let Some(r) = unsafe { rdrand64() } {
            hasher.update(r.to_le_bytes());
        }
    }

    hasher.finalize().into()
}

/// Fills `[ptr, ptr+len)` with real, cryptographically-mixed random bytes -- backs oxfs's
/// synthetic `/dev/random`/`/dev/urandom` (see this module's own doc comment for the full design).
///
/// `pub`, not `pub(crate)` -- same "kept public for test use" precedent `syscall::
/// oxidebsd_sys_read`/`oxidebsd_sys_write` already have; `tests/random_smoke.rs` calls this
/// directly (oxfs's own `/dev` wiring can only really be exercised by a real process actually
/// opening the path, which needs a full module-loading boot -- this covers the new, riskier logic
/// this pass actually added).
pub extern "C" fn oxidebsd_random_bytes(ptr: u64, len: u64) -> i64 {
    let key = gather_seed();
    let nonce = [0u8; 12];
    let mut cipher = ChaCha20::new((&key).into(), (&nonce).into());

    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has -- ptr isn't checked against the caller's actual mappings first.
    let buf = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
    buf.fill(0);
    cipher.apply_keystream(buf);
    len as i64
}

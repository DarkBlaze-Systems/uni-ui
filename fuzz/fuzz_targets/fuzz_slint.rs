//! Fuzz `slint_import::parse` — a clean-room Slint DSL importer is a trust
//! boundary; arbitrary text must come back as `Ok` or `Err`, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    // The contract: `parse` is total over `&str`. A panic here is the bug we
    // hunt. Forcing the discriminant ensures the call is never optimized away.
    let _returned = slint_import::parse(data).is_ok();
});

#![no_main]

use libfuzzer_sys::fuzz_target;
use zflow::wire::{Family, decode_family};

// Desktop messages carry JSON, the only JSON parser on the control stream.
fuzz_target!(|data: &[u8]| {
    let _ = decode_family(data, Family::Desktop);
});

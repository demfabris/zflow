#![no_main]

use libfuzzer_sys::fuzz_target;
use zflow::wire::{Family, decode_family};

fuzz_target!(|data: &[u8]| {
    let _ = decode_family(data, Family::Pairing);
});

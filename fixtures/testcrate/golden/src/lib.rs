//! Named `sample-tests`, so the name rule calls it test support — and wrong.
//! `sample-prod` lists it under `[dependencies]`, which is hard evidence that
//! production code reaches it. The graph's verdict wins over the name.

pub fn threshold() -> u32 {
    64
}

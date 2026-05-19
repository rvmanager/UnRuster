//! Fixture exercising the `unsafe_no_safety_doc` lint.

/// Documented but missing the safety section — should fire (documented variant).
///
/// Reads the byte at the given index without bounds checking.
pub unsafe fn read_at(p: *const u8, i: usize) -> u8 {
    unsafe { *p.add(i) }
}

// No doc comment at all — should fire (undocumented variant).
pub unsafe fn write_at(p: *mut u8, i: usize, val: u8) {
    unsafe { *p.add(i) = val }
}

/// Proper rustdoc: includes a Safety section — should NOT fire.
///
/// # Safety
///
/// `p` must be a valid pointer to at least `len` initialized bytes.
pub unsafe fn slice_from_raw(p: *const u8, len: usize) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts(p, len) }
}

// Private unsafe fn — should NOT fire.
#[allow(dead_code)]
unsafe fn internal_helper(_p: *const u8) {}

/// Safe public fn — should NOT fire (not unsafe).
pub fn safe() {}

pub struct Buf;

impl Buf {
    /// Reads the byte at `i`. Missing safety section — should fire.
    pub unsafe fn peek(&self, i: usize) -> u8 {
        let _ = i;
        0
    }

    /// # Safety
    ///
    /// `i` must be less than `self.len()`.
    pub unsafe fn peek_doc(&self, i: usize) -> u8 {
        let _ = i;
        0
    }
}

pub trait RawAccess {
    // Trait method, no doc — should fire.
    unsafe fn get(&self, i: usize) -> u8;

    /// # Safety
    ///
    /// `i` must be in bounds.
    unsafe fn get_doc(&self, i: usize) -> u8;
}

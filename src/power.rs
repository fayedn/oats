//! Unprivileged, process-owned idle-sleep assertions.
use anyhow::{Result, ensure};
use std::ffi::{CString, c_void};
type Ref = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: Ref, text: *const libc::c_char, encoding: u32) -> Ref;
    fn CFRelease(value: Ref);
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(kind: Ref, level: u32, name: Ref, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
fn string(text: &str) -> Result<Owned> {
    let c = CString::new(text)?;
    let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x08000100) };
    ensure!(!r.is_null(), "could not allocate CFString");
    Ok(Owned(r))
}
/// A process-owned assertion: macOS also releases it when the worker exits or crashes.
/// Prevents idle sleep only; lid closure and explicit sleep remain effective.
pub struct IdleAssertion(u32);
impl IdleAssertion {
    pub fn new() -> Result<Self> {
        let kind = string("PreventUserIdleSystemSleep")?;
        let name = string("oats running job")?;
        let mut id = 0;
        let rc = unsafe { IOPMAssertionCreateWithName(kind.0, 255, name.0, &mut id) };
        ensure!(rc == 0, "could not acquire idle assertion: {rc}");
        Ok(Self(id))
    }
}
impl Drop for IdleAssertion {
    fn drop(&mut self) {
        unsafe {
            IOPMAssertionRelease(self.0);
        }
    }
}

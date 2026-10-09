//! Exact UTC power events; no local-time formatting or subprocess timezone dependency.
use anyhow::{Result, ensure};
use std::ffi::{CString, c_void};
type Ref = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: Ref, text: *const libc::c_char, encoding: u32) -> Ref;
    fn CFDateCreate(allocator: Ref, seconds: f64) -> Ref;
    fn CFRelease(value: Ref);
    fn CFArrayGetCount(value: Ref) -> isize;
    fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    fn CFDictionaryGetValue(dict: Ref, key: Ref) -> Ref;
    fn CFEqual(a: Ref, b: Ref) -> u8;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMSchedulePowerEvent(date: Ref, owner: Ref, kind: Ref) -> i32;
    fn IOPMCancelScheduledPowerEvent(date: Ref, owner: Ref, kind: Ref) -> i32;
    fn IOPMCopyScheduledPowerEvents() -> Ref;
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
fn exists(date: &Owned, owner: &Owned, kind: &Owned) -> Result<bool> {
    let events = Owned(unsafe { IOPMCopyScheduledPowerEvents() });
    if events.0.is_null() {
        return Ok(false);
    }
    let keys = [
        string("time")?,
        string("scheduledby")?,
        string("eventtype")?,
    ];
    for index in 0..unsafe { CFArrayGetCount(events.0) } {
        let event = unsafe { CFArrayGetValueAtIndex(events.0, index) };
        let expected = [date.0, owner.0, kind.0];
        let matched = keys.iter().zip(expected).all(|(key, value)| {
            let actual = unsafe { CFDictionaryGetValue(event, key.0) };
            !actual.is_null() && unsafe { CFEqual(actual, value) } != 0
        });
        if matched {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn event(timestamp: i64, id: &str, cancel: bool) -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "power events require root");
    uuid::Uuid::parse_str(id)?;
    let date = Owned(unsafe { CFDateCreate(std::ptr::null(), timestamp as f64 - 978307200.0) });
    ensure!(!date.0.is_null(), "could not allocate CFDate");
    let owner = string(&format!("dev.oats.{id}"))?;
    let kind = string("wake")?;
    // Absence is success for cancellation: retries after partial cleanup are safe.
    if cancel && !exists(&date, &owner, &kind)? {
        return Ok(());
    }
    let rc = unsafe {
        if cancel {
            IOPMCancelScheduledPowerEvent(date.0, owner.0, kind.0)
        } else {
            IOPMSchedulePowerEvent(date.0, owner.0, kind.0)
        }
    };
    if rc != 0 && cancel && !exists(&date, &owner, &kind)? {
        return Ok(());
    }
    ensure!(rc == 0, "power event operation failed: 0x{:08x}", rc as u32);
    Ok(())
}

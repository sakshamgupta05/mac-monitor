//! Minimal IOKit bindings. Adapted from macmon (MIT, https://github.com/vladkens/macmon).

use std::ffi::{CStr, c_char, c_int};

use core_foundation::base::{CFAllocatorRef, CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
#[rustfmt::skip]
unsafe extern "C" {
  fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
  fn IOServiceGetMatchingServices(main_port: u32, matching: CFDictionaryRef, existing: *mut u32) -> c_int;
  fn IORegistryEntryGetChildIterator(entry: u32, plane: *const c_char, iterator: *mut u32) -> c_int;
  fn IORegistryEntryCreateCFProperty(entry: u32, key: CFStringRef, allocator: CFAllocatorRef, options: u32) -> CFTypeRef;
  fn IOIteratorNext(iterator: u32) -> u32;
  fn IOIteratorIsValid(iterator: u32) -> c_int;
  fn IOObjectRelease(object: u32) -> u32;
}

/// IOKit object handle, released on drop.
pub struct IoObject(u32);

impl Drop for IoObject {
  fn drop(&mut self) {
    if self.0 != 0 {
      unsafe { IOObjectRelease(self.0) };
    }
  }
}

impl IoObject {
  /// Copy of a registry entry property, None when it's missing.
  pub fn property(&self, key: &CFString) -> Option<CFType> {
    let key = key.as_concrete_TypeRef();
    let value = unsafe { IORegistryEntryCreateCFProperty(self.0, key, kCFAllocatorDefault, 0) };
    (!value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
  }

  /// Children in the service plane.
  pub fn children(&self) -> Option<IoIter> {
    let mut iter = 0;
    let ret = unsafe { IORegistryEntryGetChildIterator(self.0, c"IOService".as_ptr(), &mut iter) };
    (ret == 0).then_some(IoIter(IoObject(iter)))
  }
}

/// IOKit iterator yielding owned objects.
pub struct IoIter(IoObject);

impl IoIter {
  /// False when the registry changed during iteration and objects may have been skipped.
  pub fn is_valid(&self) -> bool {
    unsafe { IOIteratorIsValid(self.0.0) != 0 }
  }
}

impl Iterator for IoIter {
  type Item = IoObject;

  fn next(&mut self) -> Option<IoObject> {
    let next = unsafe { IOIteratorNext(self.0.0) };
    (next != 0).then_some(IoObject(next))
  }
}

/// Services of an IOKit class, its subclasses included.
pub fn matching_services(class: &CStr) -> Option<IoIter> {
  let matching = unsafe { IOServiceMatching(class.as_ptr()) };
  if matching.is_null() {
    return None;
  }
  let mut iter = 0;
  // Takes ownership of `matching`.
  let ret = unsafe { IOServiceGetMatchingServices(0, matching, &mut iter) };
  (ret == 0).then_some(IoIter(IoObject(iter)))
}

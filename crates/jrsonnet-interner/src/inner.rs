use std::{
	alloc::{self, Layout},
	borrow::Borrow,
	cmp,
	hash::{Hash, Hasher},
	mem,
	ptr::{self, NonNull},
	slice, str,
};

const UTF8_MASK: u32 = 1 << 31;
const REFCNT_MASK: u32 = !UTF8_MASK;

#[repr(C)]
struct InnerHeader {
	size: u32,
	// MSB is checked utf8 flag, rest - refcnt
	utf8_refcnt: u32,
}
impl InnerHeader {
	const fn new(size: u32, is_utf8: bool) -> Self {
		Self {
			size,
			utf8_refcnt: 1 | (if is_utf8 { UTF8_MASK } else { 0 }),
		}
	}

	const fn refcnt(&self) -> u32 {
		self.utf8_refcnt & REFCNT_MASK
	}
	const fn is_utf8(&self) -> bool {
		self.utf8_refcnt & UTF8_MASK != 0
	}

	fn set_refcnt(&mut self, cnt: u32) {
		assert_eq!(cnt & UTF8_MASK, 0);
		// Reset all bits expect last
		self.utf8_refcnt &= UTF8_MASK;
		// Store refcnt
		self.utf8_refcnt |= cnt;
	}
	fn set_is_utf8(&mut self) {
		self.utf8_refcnt |= UTF8_MASK;
	}
}

/// Similar to Rc<[u8]>, but stores all data (refcnt, size) inline, instead of being DST
pub struct Inner(NonNull<u8>);
impl Inner {
	/// # Safety
	/// `is_utf8` should only be set if data is really checked to be utf8
	/// # Panics
	/// If data is larger than 4GB
	// we allocate with correct alignment
	#[allow(clippy::cast_ptr_alignment)]
	unsafe fn new_raw(bytes: &[u8], is_utf8: bool) -> Self {
		// SAFETY:
		// - layout has non-zero size, and correct align
		// - data is written right after allocation
		// - new allocation can't overlap with passed slice
		unsafe {
			let data = alloc::alloc(Layout::from_size_align_unchecked(
				mem::size_of::<InnerHeader>() + bytes.len(),
				mem::align_of::<InnerHeader>(),
			));
			assert!(!data.is_null());
			*data.cast::<InnerHeader>() =
				InnerHeader::new(bytes.len().try_into().expect("bytes > 4GB"), is_utf8);
			ptr::copy_nonoverlapping(
				bytes.as_ptr(),
				data.add(mem::size_of::<InnerHeader>()),
				bytes.len(),
			);
			Self(NonNull::new_unchecked(data))
		}
	}
	pub fn new_bytes(bytes: &[u8]) -> Self {
		// SAFETY: is_utf8 is not set
		unsafe { Self::new_raw(bytes, false) }
	}
	#[allow(dead_code)]
	pub fn new_str(str: &str) -> Self {
		// SAFETY: strings always utf8
		unsafe { Self::new_raw(str.as_bytes(), true) }
	}

	pub fn as_slice(&self) -> &[u8] {
		let header = Self::header(self);
		// SAFETY: data is not null, and it is correctly initialized
		let size = unsafe { (*header).size };
		// SAFETY: bytes after data is allocated to be exactly data.size in length
		unsafe {
			slice::from_raw_parts(
				self.0.as_ptr().add(mem::size_of::<InnerHeader>()),
				size as usize,
			)
		}
	}

	/// # Safety
	/// Data should be checked to be utf8 via [`check_utf8`] first
	pub unsafe fn as_str_unchecked(&self) -> &str {
		// SAFETY: data is checked
		unsafe { str::from_utf8_unchecked(self.as_slice()) }
	}

	/// Check data to be utf-8
	///
	/// Positive results are cached
	pub fn check_utf8(this: &Self) -> bool {
		let header = Self::header_mut(this);
		// SAFETY: header is initialized
		if unsafe { (*header).is_utf8() } {
			return true;
		}

		if str::from_utf8(this.as_slice()).is_ok() {
			// SAFETY: header is initialized
			unsafe { (*header).set_is_utf8() };
			true
		} else {
			false
		}
	}

	/// Marks data as utf-8
	///
	/// # Safety
	/// data should be really utf-8
	pub unsafe fn assume_utf8(this: &Self) {
		let header = Self::header_mut(this);
		// SAFETY: header is correct
		unsafe { (*header).set_is_utf8() }
	}

	const fn header(this: &Self) -> *const InnerHeader {
		// in `new`, we allocate with correct alignment
		#![allow(clippy::cast_ptr_alignment)]
		this.0.as_ptr() as *const InnerHeader
	}
	const fn header_mut(this: &Self) -> *mut InnerHeader {
		// in `new`, we allocate with correct alignment
		#![allow(clippy::cast_ptr_alignment)]
		this.0.as_ptr().cast::<InnerHeader>()
	}

	fn clone(this: &Self) -> Self {
		let header = Self::header_mut(this);
		// SAFETY: header is initialized
		unsafe {
			let refcnt = (*header).refcnt() + 1;
			(*header).set_refcnt(refcnt);
		}
		Self(this.0)
	}

	pub fn ptr_eq(a: &Self, b: &Self) -> bool {
		a.0 == b.0
	}
	pub const fn as_ptr(this: &Self) -> *const u8 {
		// SAFETY: data is initialized
		unsafe { this.0.as_ptr().add(mem::size_of::<InnerHeader>()) }
	}

	pub const fn strong_count(this: &Self) -> u32 {
		let header = Self::header(this);
		// SAFETY: header is initialized
		unsafe { (*header).refcnt() }
	}
}

impl Clone for Inner {
	fn clone(&self) -> Self {
		Self::clone(self)
	}
}

impl Drop for Inner {
	fn drop(&mut self) {
		#[cold]
		#[inline(never)]
		fn dealloc(val: &Inner) {
			let header = Inner::header_mut(val);
			// SAFETY: size is correct, layout is valid
			unsafe {
				alloc::dealloc(
					val.0.as_ptr(),
					Layout::from_size_align_unchecked(
						mem::size_of::<InnerHeader>() + (*header).size as usize,
						mem::align_of::<InnerHeader>(),
					),
				);
			}
		}
		let header = Self::header_mut(self);
		// SAFETY: header is initialized
		let refcnt = unsafe {
			let refcnt = (*header).refcnt() - 1;
			(*header).set_refcnt(refcnt);
			refcnt
		};
		if refcnt == 0 {
			dealloc(self);
		}
	}
}

impl PartialEq for Inner {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0 || self.as_slice().eq(other.as_slice())
	}
}
impl Hash for Inner {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.as_slice().hash(state);
	}
}
impl Eq for Inner {}
impl PartialOrd for Inner {
	fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
		self.as_slice().partial_cmp(other.as_slice())
	}
}
impl Ord for Inner {
	fn cmp(&self, other: &Self) -> cmp::Ordering {
		self.as_slice().cmp(other.as_slice())
	}
}

impl Borrow<[u8]> for Inner {
	fn borrow(&self) -> &[u8] {
		self.as_slice()
	}
}

//! MIT-SHM capture buffer: a segment the X server creates and this process maps read-only.
#![allow(unsafe_code)]

use std::{ffi::c_void, ptr::NonNull, sync::Arc};

use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use x11rb::{
    connection::Connection,
    protocol::shm::{ConnectionExt as _, Seg},
    rust_connection::RustConnection,
};

use crate::XResult;

/// A shared segment of at least `len` bytes, detached and unmapped on drop.
pub struct ShmSegment {
    conn: Arc<RustConnection>,
    seg: Seg,
    ptr: NonNull<c_void>,
    len: usize,
}

impl ShmSegment {
    /// Asks the server for a segment (MIT-SHM 1.2 `CreateSegment`) and maps it.
    pub fn create(conn: &Arc<RustConnection>, len: usize) -> XResult<Self> {
        let seg = conn.generate_id()?;
        let size = u32::try_from(len).map_err(|_| "capture segment too large")?;
        let reply = conn.shm_create_segment(seg, size, false)?.reply()?;
        // SAFETY: a fresh shared, read-only mapping of the server's segment fd. Nothing else in
        // this process aliases it, and it is unmapped exactly once in `Drop`.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::SHARED,
                &reply.shm_fd,
                0,
            )?
        };
        let ptr = NonNull::new(ptr).ok_or("mmap returned null")?;
        Ok(Self {
            conn: Arc::clone(conn),
            seg,
            ptr,
            len,
        })
    }

    /// The segment ID for `ShmGetImage`.
    pub fn seg(&self) -> Seg {
        self.seg
    }

    /// Mapped size in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// The first `len` bytes. Call only after the `ShmGetImage` reply that filled them: the
    /// server writes the segment only while it handles our own requests, so no write overlaps
    /// the returned borrow.
    pub fn bytes(&self, len: usize) -> &[u8] {
        assert!(len <= self.len, "read past the capture segment");
        // SAFETY: `ptr` maps `self.len` readable bytes for the lifetime of `self`; see above for
        // why the server does not write while the slice lives.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().cast::<u8>(), len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are the mapping made in `create`; no borrows outlive `self`.
        let _ = unsafe { munmap(self.ptr.as_ptr(), self.len) };
        let _ = self.conn.shm_detach(self.seg);
    }
}

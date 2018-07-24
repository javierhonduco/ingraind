use {LoadError, Map, Result, VoidPtr};

use std::cell::RefCell;
use std::io;
use std::mem;
use std::os::unix::io::RawFd;
use std::ptr::null_mut;
use std::slice;
use std::sync::atomic::{self, AtomicPtr, Ordering};

use libc::{
    c_void, close, ioctl, mmap, munmap, syscall, sysconf, SYS_perf_event_open, MAP_FAILED,
    MAP_SHARED, PROT_READ, PROT_WRITE, _SC_PAGESIZE,
};

use sys::perf::*;

unsafe fn open_perf_buffer(pid: i32, cpu: i32, group: RawFd, flags: u32) -> Result<RawFd> {
    let mut attr = mem::zeroed::<perf_event_attr>();

    attr.config = perf_sw_ids_PERF_COUNT_SW_BPF_OUTPUT as u64;
    attr.size = mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = perf_type_id_PERF_TYPE_SOFTWARE;
    attr.sample_type = perf_event_sample_format_PERF_SAMPLE_RAW as u64;
    attr.__bindgen_anon_1.sample_period = 1;
    attr.__bindgen_anon_2.wakeup_events = 1;

    let pfd = syscall(
        SYS_perf_event_open,
        &attr as *const perf_event_attr,
        pid,
        cpu,
        group,
        flags | PERF_FLAG_FD_CLOEXEC,
    );
    if pfd < 0 {
        Err(LoadError::IO(io::Error::last_os_error()))
    } else {
        Ok(pfd as RawFd)
    }
}

#[repr(C)]
pub struct Sample {
    header: perf_event_header,
    pub size: u32,
    pub data: *const u8,
}

#[repr(C)]
pub struct LostSamples {
    header: perf_event_header,
    pub id: u64,
    pub count: u64,
}

pub enum Event<'a> {
    Sample(&'a Sample),
    Lost(&'a LostSamples),
}

pub struct PerfMap {
    base_ptr: AtomicPtr<perf_event_mmap_page>,
    page_cnt: u32,
    page_size: u32,
    mmap_size: usize,
    buf: RefCell<Vec<u8>>,
    pub fd: RawFd,
}

impl PerfMap {
    pub fn bind(
        map: &mut Map,
        pid: i32,
        mut cpu: i32,
        page_cnt: u32,
        group: RawFd,
        flags: u32,
    ) -> Result<PerfMap> {
        unsafe {
            let mut fd = open_perf_buffer(pid, cpu, group, flags)?;
            let page_size = sysconf(_SC_PAGESIZE) as u32;
            let mmap_size = (page_size * (page_cnt + 1)) as usize;
            let base_ptr = mmap(
                null_mut(),
                mmap_size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            );

            if base_ptr == MAP_FAILED {
                return Err(LoadError::IO(io::Error::last_os_error()));
            }

            if ioctl(fd, PERF_EVENT_IOC_ENABLE, 0) != 0 {
                return Err(LoadError::IO(io::Error::last_os_error()));
            }

            map.set(
                &mut cpu as *mut i32 as VoidPtr,
                &mut fd as *mut i32 as VoidPtr,
            );

            Ok(PerfMap {
                base_ptr: AtomicPtr::new(base_ptr as *mut perf_event_mmap_page),
                buf: RefCell::new(vec![]),
                page_cnt,
                page_size,
                mmap_size,
                fd,
            })
        }
    }

    pub fn read(&self) -> Option<Event> {
        unsafe {
            let header = self.base_ptr.load(Ordering::SeqCst);
            let data_head = (*header).data_head;
            let data_tail = (*header).data_tail;
            let raw_size = (self.mmap_size - 1) as u64;
            let base = header.offset(self.page_size as isize) as *const u8;

            if data_tail == data_head {
                return None;
            }

            let start = (data_tail % raw_size) as isize;
            let event = base.offset(start) as *const perf_event_header;
            let end = ((data_tail + (*event).size as u64) % raw_size) as isize;

            let mut buf = self.buf.borrow_mut();
            buf.clear();

            if end < start {
                let len = (raw_size as isize - start) as usize;
                let ptr = base.offset(start);
                buf.extend_from_slice(slice::from_raw_parts(ptr, len));

                let len = (*event).size as usize - len;
                let ptr = base;
                buf.extend_from_slice(slice::from_raw_parts(ptr, len));
            } else {
                let ptr = base.offset(start);
                let len = (*event).size as usize;
                buf.extend_from_slice(slice::from_raw_parts(ptr, len));
            }

            atomic::fence(Ordering::SeqCst);
            (*header).data_tail += (*event).size as u64;

            match (*event).type_ {
                perf_event_type_PERF_RECORD_SAMPLE => {
                    Some(Event::Sample(&*(buf.as_ptr() as *const Sample)))
                }
                perf_event_type_PERF_RECORD_LOST => {
                    Some(Event::Lost(&*(buf.as_ptr() as *const LostSamples)))
                }
                _ => None,
            }
        }
    }
}

impl Drop for PerfMap {
    fn drop(&mut self) {
        unsafe {
            munmap(
                self.base_ptr.load(Ordering::SeqCst) as *mut c_void,
                self.mmap_size,
            );
            ioctl(self.fd, PERF_EVENT_IOC_DISABLE, 0);
            close(self.fd);
        }
    }
}

//! 跨进程共享内存总线。
//!
//! 把 [`SpscRingBuffer`] 的完整内存布局原样放进一块 POSIX 共享内存
//! （`shm_open` + `ftruncate` + `mmap`），两个进程各自 `mmap` 同一块区域后，
//! 生产者在一端 `push`、消费者在另一端 `pop`，延迟与进程内队列同量级，
//! 且天然适合挂一个独立的监控 / 落盘进程。
//!
//! 为零第三方依赖，这里用 `extern "C"` 直接声明需要的 POSIX 函数
//! （Linux 上链接系统 libc 即可），不引入 `libc` / `memmap2` 等 crate。
//!
//! # 使用约定
//!
//! * 一个进程调用 [`SharedMemoryBus::create`] 建区并初始化（`O_CREAT|O_EXCL`）；
//! * 其他进程调用 [`SharedMemoryBus::open`] 挂载已存在的区；
//! * **创建方必须先完成初始化，对方再 `open`**（否则读到全零的空队列，
//!   语义上仍安全——只是会丢掉建区前的数据）；
//! * 只有创建方 `Drop` 时会 `shm_unlink`，引用计数式的生命周期管理
//!   留给部署层。

use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

use super::spsc::SpscRingBuffer;
use legos_core::MessageBus;

// --- 直接声明的 POSIX 接口（x86_64 Linux ABI） -------------------------------

extern "C" {
    fn shm_open(name: *const c_char, oflag: c_int, mode: c_uint) -> c_int;
    fn shm_unlink(name: *const c_char) -> c_int;
    fn ftruncate(fd: c_int, length: i64) -> c_int;
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> c_int;
    fn close(fd: c_int) -> c_int;
}

const O_CREAT: c_int = 0o100;
const O_EXCL: c_int = 0o200;
const O_RDWR: c_int = 0o2;
const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const MAP_SHARED: c_int = 0x01;
const MAP_FAILED: *mut c_void = -1isize as *mut c_void;

/// 跨进程 SPSC 总线：`T` 为元素类型，`CAP` 为容量。
pub struct SharedMemoryBus<T: Copy, const CAP: usize> {
    base: *mut c_void,
    len: usize,
    ring: *mut SpscRingBuffer<T, CAP>,
    name: CString,
    /// 是否为创建方（只有创建方在 Drop 时 unlink）。
    owner: bool,
    _marker: PhantomData<SpscRingBuffer<T, CAP>>,
}

impl<T: Copy, const CAP: usize> SharedMemoryBus<T, CAP> {
    fn ring_bytes() -> usize {
        std::mem::size_of::<SpscRingBuffer<T, CAP>>()
    }

    fn cname(name: &str) -> Result<CString, String> {
        let n = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/{name}")
        };
        CString::new(n).map_err(|e| format!("shm name 含 NUL 字节: {e}"))
    }

    fn map_fd(fd: c_int, len: usize) -> Result<*mut c_void, String> {
        // SAFETY: fd 是刚 shm_open/ftruncate 好的有效描述符，len > 0。
        let base = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
        // fd 在 mmap 成功后即可关闭，映射不受影响。
        unsafe {
            close(fd);
        }
        if base == MAP_FAILED || base.is_null() {
            return Err("mmap 失败".to_string());
        }
        Ok(base)
    }

    /// 创建共享内存区并初始化队列。区已存在时返回 `Err`。
    pub fn create(name: &str, _perm: u32) -> Result<Self, String> {
        let cname = Self::cname(name)?;
        let len = Self::ring_bytes();
        // SAFETY: 参数均为合法 POSIX 参数；O_EXCL 保证只由我们初始化。
        let fd = unsafe { shm_open(cname.as_ptr(), O_RDWR | O_CREAT | O_EXCL, 0o600) };
        if fd < 0 {
            return Err(format!("shm_open(O_CREAT|O_EXCL) 失败: {name}（可能已存在）"));
        }
        if unsafe { ftruncate(fd, len as i64) } != 0 {
            unsafe {
                close(fd);
                shm_unlink(cname.as_ptr());
            }
            return Err("ftruncate 失败".to_string());
        }
        let base = Self::map_fd(fd, len)?;
        let ring = base as *mut SpscRingBuffer<T, CAP>;
        // SAFETY: 独占新建的映射，写入初始化值安全。
        unsafe {
            ptr::write(ring, SpscRingBuffer::new());
        }
        Ok(Self {
            base,
            len,
            ring,
            name: cname,
            owner: true,
            _marker: PhantomData,
        })
    }

    /// 挂载已存在的共享内存区。
    pub fn open(name: &str) -> Result<Self, String> {
        let cname = Self::cname(name)?;
        let len = Self::ring_bytes();
        // SAFETY: 只读/读写打开已存在的 shm 对象。
        let fd = unsafe { shm_open(cname.as_ptr(), O_RDWR, 0o600) };
        if fd < 0 {
            return Err(format!("shm_open 失败: {name}（不存在或无权限）"));
        }
        let base = Self::map_fd(fd, len)?;
        Ok(Self {
            base,
            len,
            ring: base as *mut SpscRingBuffer<T, CAP>,
            name: cname,
            owner: false,
            _marker: PhantomData,
        })
    }

    /// 底层环形队列引用（生产者 / 消费者都通过它操作）。
    pub fn ring(&self) -> &SpscRingBuffer<T, CAP> {
        // SAFETY: 映射在 Self 存活期间有效；SPSC 协议保证并发 push/pop 安全。
        unsafe { &*self.ring }
    }
}

// 原子操作跨进程可见（x86_64 上对齐的原子读写天然跨进程一致）。
unsafe impl<T: Copy + Send, const CAP: usize> Send for SharedMemoryBus<T, CAP> {}
unsafe impl<T: Copy + Send, const CAP: usize> Sync for SharedMemoryBus<T, CAP> {}

impl<T: Copy, const CAP: usize> Drop for SharedMemoryBus<T, CAP> {
    fn drop(&mut self) {
        unsafe {
            munmap(self.base, self.len);
            if self.owner {
                shm_unlink(self.name.as_ptr());
            }
        }
    }
}

impl<T: Copy, const CAP: usize> MessageBus for SharedMemoryBus<T, CAP> {
    type Item = T;
    fn push(&self, item: T) -> bool {
        self.ring().push(item)
    }
    fn pop(&self) -> Option<T> {
        self.ring().pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        format!(
            "legos_test_{}_{}",
            tag,
            std::process::id()
        )
    }

    #[test]
    fn create_open_share_between_handles() {
        // 同一进程内两个句柄映射同一块 shm：一端 push、一端 pop，
        // 验证“共享内存”语义（跨进程时只是把 open 放到另一个进程里）。
        let name = unique_name("spsc");
        let creator = SharedMemoryBus::<u64, 64>::create(&name, 0o600).unwrap();
        let opener = SharedMemoryBus::<u64, 64>::open(&name).unwrap();

        for i in 0..50u64 {
            assert!(creator.push(i));
        }
        for i in 0..50u64 {
            assert_eq!(opener.pop(), Some(i));
        }
        // 反方向也通（SPSC 是单向的，这里只是验证映射双向可见）。
        assert!(opener.push(777));
        assert_eq!(creator.pop(), Some(777));

        drop(opener);
        drop(creator); // owner unlink
        // 区已被 unlink，再 open 应失败。
        assert!(SharedMemoryBus::<u64, 64>::open(&name).is_err());
    }

    #[test]
    fn create_twice_fails() {
        let name = unique_name("excl");
        let _a = SharedMemoryBus::<u64, 8>::create(&name, 0o600).unwrap();
        assert!(SharedMemoryBus::<u64, 8>::create(&name, 0o600).is_err());
    }

    #[test]
    fn open_missing_fails() {
        assert!(SharedMemoryBus::<u64, 8>::open("legos_test_definitely_missing_xyz").is_err());
    }

    #[test]
    fn tick_roundtrip_through_shm() {
        use legos_core::{EventKind, Side, Tick};
        let name = unique_name("tick");
        let bus = SharedMemoryBus::<Tick, 16>::create(&name, 0o600).unwrap();
        let t = Tick {
            symbol_id: 9,
            price: 123_4567,
            qty: 42,
            side: Side::Ask,
            kind: EventKind::Trade,
            order_id: 5,
            ts_ns: 999,
        };
        assert!(bus.push(t));
        assert_eq!(bus.pop(), Some(t));
    }
}

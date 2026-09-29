//! legos-bus: 无锁数据总线 —— 全系统的「血管」。
//!
//! * [`SpscRingBuffer`]: 进程内单生产者 / 单消费者无锁环形队列。
//!   `[MaybeUninit<T>; CAP]` 定长数组 + 原子 `head`/`tail`，零堆分配。
//! * [`SharedMemoryBus`]: 把同样的 SPSC 布局搬进 Linux 共享内存（`shm_open` +
//!   `mmap`），实现跨进程行情分发。仅用 `extern "C"` 直接声明 POSIX 接口，
//!   不依赖任何第三方 crate。

mod shm;
mod spsc;

pub use shm::SharedMemoryBus;
pub use spsc::SpscRingBuffer;

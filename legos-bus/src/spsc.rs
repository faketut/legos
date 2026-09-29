//! 单生产者 / 单消费者（SPSC）无锁环形队列。
//!
//! 内存布局契约（见 `legos-bus/SPEC.md`）：
//!
//! ```text
//! 偏移            字段
//! 0     64B     head: CachePadded<AtomicUsize>   消费者下标（独占缓存行）
//! 64    64B     tail: CachePadded<AtomicUsize>   生产者下标（独占缓存行）
//! 128   CAP*T   buf: [MaybeUninit<T>; CAP]       定长槽位数组
//! ```
//!
//! `head` 与 `tail` 各自 `#[repr(align(64))]` 独占一个缓存行——生产者只写
//! `tail`、消费者只写 `head`，两者永不在同一缓存行上互相 invalidate
//! （防伪共享）。整个结构体可原样搬进共享内存（见 [`crate::SharedMemoryBus`]）。
//!
//! 同步协议（经典 SPSC，无锁、无等待）:
//!
//! * 生产者 `push`：先 `Acquire` 读 `head` 判断是否满；写入槽位后
//!   `Release` 发布 `tail`。消费者看到新 `tail` 时，槽位写入已可见。
//! * 消费者 `pop`：先 `Acquire` 读 `tail` 判断是否空；读出槽位后
//!   `Release` 发布 `head`。生产者看到新 `head` 时，槽位可安全复用。
//!
//! 下标用 `wrapping_add` 递增，`usize` 回绕后 `tail - head` 的差值语义不变。

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use legos_core::MessageBus;

/// 把一个值独占一个缓存行。`#[repr(align(64))]` 保证地址 64 字节对齐、
///
/// 结构体大小至少 64 字节——相邻的两个 `CachePadded` 不可能落在同一缓存行。
#[derive(Debug)]
#[repr(align(64))]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self { value }
    }
    pub fn get(&self) -> &T {
        &self.value
    }
}

/// SPSC 无锁环形队列。`T: Copy`（无 `Drop`，槽位复用 sound），`CAP` 为槽位数。
pub struct SpscRingBuffer<T: Copy, const CAP: usize> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    buf: [MaybeUninit<T>; CAP],
}

impl<T: Copy, const CAP: usize> SpscRingBuffer<T, CAP> {
    /// `const` 构造：可用于 `static` 初始化，也可直接 `ptr::write` 进共享内存。
    pub const fn new() -> Self {
        assert!(CAP > 0, "SpscRingBuffer capacity must be > 0");
        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            buf: [MaybeUninit::uninit(); CAP],
        }
    }

    /// 容量（槽位数）。
    pub const fn capacity(&self) -> usize {
        CAP
    }

    /// 当前队列中元素个数（并发调用者看到的值是近似的，仅用于监控）。
    pub fn len(&self) -> usize {
        let tail = self.tail.get().load(Ordering::Relaxed);
        let head = self.head.get().load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 生产者：入队。队满返回 `false`（不阻塞、不分配、不覆盖旧数据）。
    pub fn push(&self, item: T) -> bool {
        let tail = self.tail.get().load(Ordering::Relaxed);
        // Acquire：与消费者 Release 发布 head 配对，确保看到槽位已空闲。
        let head = self.head.get().load(Ordering::Acquire);
        if tail.wrapping_sub(head) == CAP {
            return false; // 满
        }
        // SAFETY: 该槽位已被消费者释放（head 已越过它），生产者独占写安全；
        // T: Copy 无 Drop，不存在重复析构问题。通过裸指针写入以绕过 `&self`
        // 的别名限制——SPSC 协议保证此槽位此刻没有其他访问者。
        unsafe {
            let slot = self.buf.as_ptr() as *mut MaybeUninit<T>;
            (*slot.add(tail % CAP)).write(item);
        }
        // Release：槽位写入先于 tail 发布，消费者 Acquire 读到新 tail
        // 时一定能看到完整写入。
        self.tail.get().store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// 消费者：出队。队空返回 `None`。
    pub fn pop(&self) -> Option<T> {
        let head = self.head.get().load(Ordering::Relaxed);
        // Acquire：与生产者 Release 发布 tail 配对，确保看到槽位完整写入。
        let tail = self.tail.get().load(Ordering::Acquire);
        if head == tail {
            return None; // 空
        }
        // SAFETY: 生产者已 Release 发布该槽位（tail 已越过它），独占读安全。
        // `assume_init_read` 按位复制出 T（T: Copy），槽位随后被生产者覆盖。
        let item = unsafe { self.buf[head % CAP].assume_init_read() };
        // Release：读出先于 head 发布，生产者看到新 head 后可安全复用槽位。
        self.head.get().store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }
}

impl<T: Copy, const CAP: usize> Default for SpscRingBuffer<T, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

// `&self` 方法内部全是原子操作：多线程共享引用安全。
unsafe impl<T: Copy + Send, const CAP: usize> Send for SpscRingBuffer<T, CAP> {}
unsafe impl<T: Copy + Send, const CAP: usize> Sync for SpscRingBuffer<T, CAP> {}

impl<T: Copy, const CAP: usize> MessageBus for SpscRingBuffer<T, CAP> {
    type Item = T;
    fn push(&self, item: T) -> bool {
        SpscRingBuffer::push(self, item)
    }
    fn pop(&self) -> Option<T> {
        SpscRingBuffer::pop(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legos_core::{EventKind, MarketTick, Side};
    use std::sync::Arc;
    use std::thread;

    fn tick(id: u32) -> MarketTick {
        MarketTick::new(1, 100_0000 + id as i64, id, Side::Bid, EventKind::Add, id, id as u64)
    }

    #[test]
    fn head_tail_no_false_sharing() {
        // 数据契约：head / tail 各自独占 64 字节缓存行。
        assert_eq!(std::mem::size_of::<CachePadded<AtomicUsize>>(), 64);
        assert_eq!(std::mem::align_of::<CachePadded<AtomicUsize>>(), 64);
        let q = SpscRingBuffer::<u64, 8>::new();
        let head_addr = q.head.get() as *const _ as usize;
        let tail_addr = q.tail.get() as *const _ as usize;
        assert!(
            head_addr.abs_diff(tail_addr) >= 64,
            "head/tail 必须分属不同缓存行"
        );
    }

    #[test]
    fn fifo_order() {
        let q = SpscRingBuffer::<MarketTick, 16>::new();
        for i in 0..10 {
            assert!(q.push(tick(i)));
        }
        for i in 0..10 {
            assert_eq!(q.pop().unwrap().order_id, i);
        }
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn full_then_drain_then_reuse() {
        let q = SpscRingBuffer::<u64, 4>::new();
        for i in 0..4 {
            assert!(q.push(i));
        }
        assert!(!q.push(99), "满队列 push 必须返回 false，不能覆盖");
        assert_eq!(q.len(), 4);
        assert_eq!(q.pop(), Some(0));
        assert!(q.push(99), "drain 一个槽位后应可继续 push");
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), Some(99));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn wraparound_many_cycles() {
        // 反复绕回，验证取模索引与 wrapping 计数器长期正确。
        let q = SpscRingBuffer::<u64, 8>::new();
        for round in 0..500u64 {
            for i in 0..8 {
                assert!(q.push(round * 8 + i));
            }
            for i in 0..8 {
                assert_eq!(q.pop(), Some(round * 8 + i));
            }
        }
        assert!(q.is_empty());
    }

    #[test]
    fn spsc_across_threads_no_loss_no_reorder() {
        const N: u64 = 200_000;
        let q = Arc::new(SpscRingBuffer::<u64, 4096>::new());
        let producer = {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                let mut sent = 0u64;
                while sent < N {
                    if q.push(sent) {
                        sent += 1;
                    } else {
                        thread::yield_now(); // 队满：自旋等待消费者
                    }
                }
            })
        };
        let mut received = 0u64;
        while received < N {
            match q.pop() {
                Some(v) => {
                    assert_eq!(v, received, "SPSC 必须保序、无丢失");
                    received += 1;
                }
                None => thread::yield_now(),
            }
        }
        producer.join().unwrap();
        assert!(q.is_empty());
    }

    #[test]
    fn message_bus_trait_object_safety_free() {
        // 通过泛型 trait 使用（非 dyn），确认单态化路径可用。
        fn drain<B: MessageBus<Item = u64>>(b: &B) -> u64 {
            let mut n = 0;
            while b.pop().is_some() {
                n += 1;
            }
            n
        }
        let q = SpscRingBuffer::<u64, 32>::new();
        for i in 0..32 {
            q.push(i);
        }
        assert_eq!(drain(&q), 32);
    }
}

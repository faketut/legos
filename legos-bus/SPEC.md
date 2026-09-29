# SPEC — legos-bus（无锁数据总线）

> 代码若与本文冲突，以本文为准做重构。
> 测试映射：`src/spsc.rs`、`src/shm.rs` 的 `#[cfg(test)]` 模块。

## 1. 数据契约（Data Spec）

### 1.1 SpscRingBuffer 内存布局

```
偏移            字段
0      64 B     head: CachePadded<AtomicUsize>   消费者下标（独占缓存行）
64     64 B     tail: CachePadded<AtomicUsize>   生产者下标（独占缓存行）
128    CAP×T    buf:  [MaybeUninit<T>; CAP]      定长槽位数组
```

- `CachePadded<T>` 为 `#[repr(align(64))]` 包装：大小 ≥ 64 字节、地址 64 字节
  对齐。生产者只写 `tail`、消费者只写 `head`，两者**永不在同一缓存行**
  上互相 invalidate（防伪共享）。单测用地址差断言 `head`/`tail` 间距 ≥ 64。
- `T: Copy`：槽位复用无 Drop 语义，按位拷贝 sound。
- `CAP ≥ 1`（`const` 断言）；满判定为 `tail − head == CAP`，`push` 满时
  返回 `false`（不阻塞、不分配、不覆盖）。
- `#[repr(C)]` 顺序布局：`SharedMemoryBus` 可把整个结构体原样搬进共享内存。

### 1.2 SharedMemoryBus ABI

- `create(name)`：`shm_open(O_CREAT|O_EXCL)` + `ftruncate(sizeof(ring))` +
  `mmap(MAP_SHARED)`，`ptr::write` 初始化；已存在则 `Err`。
- `open(name)`：挂载已存在区域；不存在则 `Err`。
- 只有创建方 `Drop` 时 `shm_unlink`；`munmap` 各自负责。
- 跨进程可见性：x86_64 上对齐的原子读写天然跨进程一致；
  Acquire/Release 语义与进程内队列完全相同。

## 2. 行为契约（Behavior Spec）

### 2.1 SPSC 同步协议（状态机）

队列状态：`空 (head == tail)` → `部分` → `满 (tail − head == CAP)`。

| 操作 | 前置 | 内存序 | 后置 |
|------|------|--------|------|
| `push` | `Acquire` 读 `head`，未满 | 槽位写入 → `Release` 存 `tail` | 消费者 `Acquire` 读到新 `tail` 时槽位完整可见 |
| `pop` | `Acquire` 读 `tail`，非空 | `assume_init_read` 按位拷贝 → `Release` 存 `head` | 生产者看到新 `head` 后可安全复用槽位 |

下标 `wrapping_add` 递增；`usize` 回绕后差值语义不变。
**契约保证**：保序、无丢失（20 万条跨线程测试）；满/空永不阻塞，
调用方负责自旋或先 drain。

### 2.2 MessageBus trait 契约

- `push(&self, item) -> bool`：`false` 仅表示“满”，无其他失败模式。
- `pop(&self) -> Option<Item>`：`None` 仅表示“空”。
- 只取 `&self`：`Send + Sync` 由内部原子操作保证（`unsafe impl` 有 SAFETY 注释）。

## 3. 测试映射

| 契约条目 | 测试 |
|----------|------|
| §1.1 防伪共享 | `head_tail_no_false_sharing`（size/align/地址差） |
| §2.1 保序无丢失 | `spsc_across_threads_no_loss_no_reorder`（20 万条） |
| §2.1 满→drain→复用 | `full_then_drain_then_reuse` |
| §2.1 回绕 | `wraparound_many_cycles`（500 轮） |
| §1.2 shm 语义 | `create_open_share_between_handles`（双句柄 push/pop） |
| §1.2 O_EXCL/缺失 | `create_twice_fails`、`open_missing_fails` |
| tick 按位往返 | `tick_roundtrip`、`tick_roundtrip_through_shm` |

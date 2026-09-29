# Legos —— 零开销编译期积木高频交易系统

> *ZeroOverhead-HFT*：把交易系统的每个组件做成编译期可替换的“乐高积木”。

本系统（代号 **Legos**）的核心架构是**基于 Rust Trait 与泛型的静态分发设计**。
六大组件全部定义为 trait，装配时只用泛型参数指定具体实现——编译期的
**单态化（Monomorphization）** 会把层层解耦的组件抹平、内联成一条直线代码，
运行期**彻底消除虚函数指针跳转（Dynamic Dispatch）**，热路径**零堆分配**，
压榨出如同手写一体化 C++ 般的零开销低延迟表现。

切换积木 = 改一行泛型参数 = 重新编译。不改逻辑代码，不碰运行时配置。

```rust
// 回测装配
Pipeline<CsvFileFeed, SpscRingBuffer<Tick, 4096>, L2FlatArrayBook<10>,
         MarketMakerStrategy, PassThroughRisk, SimulatedExchange>
// 实盘装配：只换泛型参数，其余代码一字不动
Pipeline<NativeItchParser, SharedMemoryBus<Tick, 4096>, L3MapBook,
         MarketMakerStrategy, HardLimitRisk, FixProtocolGateway>
```

## 四步开发流水线

按“由内到外、由低向高”的顺序构建（若先写策略/网关会导致 Trait 后期大规模重构）：

| Phase | 内容 | 交付 crate |
|-------|------|-----------|
| 1. 数据总线与基础设施 | 先确立核心数据结构 `Tick`、`Order`（定长、`Copy`，热路径零堆分配），再实现 `SpscRingBuffer`（手写无锁队列：`MaybeUninit` 数组 + 原子 `head`/`tail`）和 `SharedMemoryBus`（Linux 共享内存跨进程）。无锁队列的并发安全是全系统的血管，最先跑通 | `legos-core`, `legos-bus` |
| 2. 高频内存账簿 | `L2FlatArrayBook`：固定连续数组上的**零堆分配二分插入/查找**维护价格梯队，L1/L2 缓存友好；`L3MapBook`：按订单 ID 的树状结构，用于深度排队分析 | `legos-book` |
| 3. 外围网关与仿真器 | `CsvFileFeed`（回测读文件）、`SimulatedExchange`（回测内存撮合）、`NativeItchParser`（实盘 NASDAQ ITCH 5.0 二进制解析）、`FixProtocolGateway`（实盘 FIX 4.4）。解析出的 Tick 丢进总线驱动账簿，形成输入闭环 | `legos-feed`, `legos-gateway`, `legos-risk` |
| 4. 策略逻辑与泛型管线编织 | `MarketMakerStrategy` 做市、`ArbitrageStrategy` 跨场所套利；`Pipeline` 主循环用一行泛型声明把 6 大组件在编译期织成直线流水线；`core_affinity` 把主循环绑定到指定物理 CPU 核心；`criterion` 全链路延迟基准 | `legos-strategy`, `legos-app` |

## Crate 地图

```
legos/
├── legos-core      # 地基：Tick/Order/OrderIntent/OrderAck（定长 Copy）+ 六大 trait
│                     # MarketDataFeed / MessageBus / OrderBook / TradingStrategy /
│                     # PreTradeRisk / ExecutionGateway —— 热路径无 dyn、无堆分配
├── legos-bus       # Phase 1：SpscRingBuffer<T: Copy, const CAP: usize>（原子 head/tail）
│                   #          SharedMemoryBus（shm_open+mmap，extern "C" 直调 POSIX，零第三方依赖）
├── legos-book      # Phase 2：L2FlatArrayBook<LEVELS>（扁平数组二分查找+copy_within 平移）
│                   #          L3MapBook（BTreeMap<u64, Order> + 价格档树）
├── legos-feed      # Phase 3：CsvFileFeed（含 data/sample_ticks.csv 示例数据）
│                   #          NativeItchParser（ITCH 5.0：'A'/'E'/'C'/'X'/'D'）
├── legos-risk      # Phase 3：PassThroughRisk（恒 Ok，单态化后零开销）
│                   #          HardLimitRisk（单笔名义金额/数量/每秒订单数硬限额）
├── legos-gateway   # Phase 3：FixProtocolGateway（FIX 4.4 NewOrderSingle，TCP，失败永不 panic）
│                   #          SimulatedExchange（内存撮合 + 滑点模型，64 槽位零堆分配报价表）
├── legos-strategy  # Phase 4：MarketMakerStrategy / ArbitrageStrategy /
│                   #          PythonBindingStrategy（默认 stub；python feature 切真实嵌入）
└── legos-app       # Phase 4：Pipeline<F, B, Bk, S, R, G> 泛型主循环 + core_affinity 绑核
                    #          + benches/pipeline_bench.rs（criterion 全链路基准）
```

## 如何替换积木块

所有替换都在**类型层面**完成。`legos-app/src/main.rs` 的 `_demo_swap_bricks_at_compile_time`
函数（虽未被 main 调用，但会被编译）就是证明：

```rust
// 回测 → 实盘：换三个泛型参数
let mut pipe: Pipeline<
    CsvFileFeed,
    SpscRingBuffer<Tick, 4096>,
    L3MapBook,            // ← L2FlatArrayBook<10> 换成 L3MapBook
    MarketMakerStrategy,
    HardLimitRisk,        // ← PassThroughRisk 换成 HardLimitRisk
    FixProtocolGateway,   // ← SimulatedExchange 换成 FixProtocolGateway
> = Pipeline::new(feed, bus, book, strategy, risk, gateway);
```

| 场景 | 行情 | 总线 | 账簿 | 策略 | 风控 | 网关 |
|------|------|------|------|------|------|------|
| 本地回测 | `CsvFileFeed` | `SpscRingBuffer` | `L2FlatArrayBook<10>` | `MarketMakerStrategy` | `PassThroughRisk` | `SimulatedExchange` |
| 实盘交易 | `NativeItchParser` | `SharedMemoryBus` | `L2FlatArrayBook<10>` | `MarketMakerStrategy` | `HardLimitRisk` | `FixProtocolGateway` |
| 深度研究 | `CsvFileFeed` | `SpscRingBuffer` | `L3MapBook` | `PythonBindingStrategy` | `PassThroughRisk` | `SimulatedExchange` |

## 构建 / 测试 / 运行

需要 Rust 1.70+（本仓库用 rustup 安装，见环境备注）：

```bash
cargo build --workspace        # 构建全部 8 个 crate
cargo test --workspace         # 运行全部单测（53 个，含多线程 SPSC 压力测试）
cargo run -p legos-app         # 回测：CSV → 总线 → 账簿 → 做市策略 → 风控 → 模拟撮合
cargo bench -p legos-app       # criterion 全链路延迟基准（feed→bus→book→strategy→risk→gateway）
```

启用 Python 嵌入（需要 Python 3.8+ 开发环境）：

```bash
cargo build -p legos-strategy --features python
```

## 基准结果

`cargo bench -p legos-app`（release，`benches/pipeline_bench.rs`），机器：x86_64 Linux：

| 基准 | 含义 | 结果 |
|------|------|------|
| `spsc_ring_buffer/push_pop_tick` | 无锁队列一推一取 | ~17.4 ns |
| `l2_book/apply_tick` | L2 账簿处理一个 Tick | ~21.7 ns |
| `full_pipeline/tick_to_fill_1000` | 全链路 1000 tick（feed→bus→book→strategy→risk→gateway） | ~47.8 µs（≈47.8 ns/tick） |

> 注：以上为开发机上的相对参考值，实盘延迟以生产环境实测为准。

## Stub 清单（诚实说明）

| 位置 | Stub 内容 | 原因 |
|------|-----------|------|
| `legos-strategy::PythonBindingStrategy`（默认构建） | `on_tick` 恒返回 `None` 的同名占位 | 避免默认构建依赖 Python 解释器；`--features python` 切换为 pyo3 真实嵌入 |
| `NativeItchParser` 未实现的消息类型（`'S'`/`'R'`/`'H'`/`'P'`/`'Q'`/`'B'`/`'U'` 等） | 按长度跳过 | 实现 `'A'`/`'E'`/`'C'`/`'X'`/`'D'` 已覆盖盘口重构所需的全部订单流；其余类型在模块文档中列表说明，可按需追加分支 |
| `SimulatedExchange` 的非交叉限价单 | 返回 `Accepted`（挂单，不建完整订单簿） | 回测撮合的最小可用语义；文档化说明 |
| `ArbitrageStrategy` | 只发出买入腿意图 | 卖出腿对冲归执行层/组合管理；文档化说明 |
| `CsvFileFeed` 的 `order_id` | 恒为 0 | CSV 格式不携带订单号；L2 账簿不依赖它，L3 场景请用 ITCH feed |

## 许可证

MIT

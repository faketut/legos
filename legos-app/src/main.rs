//! legos-app: 把六大积木在**编译期**织成一条直线流水线的二进制。
//!
//! ```text
//! feed.next_event() → bus.push() → bus.pop() → book.apply()
//!     → strategy.on_tick() → risk.check_order() → gateway.send_order()
//! ```
//!
//! `Pipeline` 的 6 个类型参数全部是**泛型**（不是 `dyn`）：
//! `cargo build` 做单态化时把每一层调用直接内联，运行时不存在任何
//! 虚函数跳转——这就是「Legos 编译期积木」名称的由来。
//!
//! 每个 `OrderIntent` 都配一个 [`TrackedOrder`](legos_core::TrackedOrder)
//! 走完状态机：`Created → PendingNew → {Accepted|Filled|PartiallyFilled|Rejected}`，
//! 任何非法转换被计数到 `PipelineStats.illegal_transitions`（正常应恒为 0）。
//!
//! 行为契约见 `legos-app/SPEC.md`。

use std::time::{SystemTime, UNIX_EPOCH};

use legos_book::{L2FlatArrayBook, L3MapBook};
use legos_bus::SpscRingBuffer;
use legos_core::{
    AckStatus, ExecutionGateway, MarketDataFeed, MarketTick, MessageBus, OrderBook, OrderState,
    PreTradeRisk, TrackedOrder, TradingStrategy,
};
use legos_feed::CsvFileFeed;
use legos_gateway::{FixProtocolGateway, SimulatedExchange};
use legos_risk::{HardLimitRisk, PassThroughRisk};
use legos_strategy::MarketMakerStrategy;

// ---------------------------------------------------------------------------
// Pipeline：六积木泛型装配
// ---------------------------------------------------------------------------

/// 全链路交易管线。类型参数即积木选型：
///
/// `F`: 行情接入，`B`: 数据总线，`Bk`: 账簿，
/// `S`: 策略，`R`: 风控，`G`: 执行网关。
pub struct Pipeline<F, B, Bk, S, R, G> {
    pub feed: F,
    pub bus: B,
    pub book: Bk,
    pub strategy: S,
    pub risk: R,
    pub gateway: G,
    /// bus 满时暂存的事件。不变量：`run()` 返回时必须为 `None`——
    /// feed 取出的事件绝不允许静默丢弃（bus 满只是背压，不是丢弃理由）。
    pending: Option<MarketTick>,
}

/// 单次运行统计。
#[derive(Debug, Default)]
pub struct PipelineStats {
    pub ticks: u64,
    pub intents: u64,
    pub sent: u64,
    pub rejected_by_risk: u64,
    pub rejected_by_gateway: u64,
    pub fills: u64,
    /// 状态机非法转换计数（契约要求恒为 0；非 0 即实现 bug）。
    pub illegal_transitions: u64,
}

impl<F, B, Bk, S, R, G> Pipeline<F, B, Bk, S, R, G>
where
    F: MarketDataFeed,
    B: MessageBus<Item = MarketTick>,
    Bk: OrderBook,
    S: TradingStrategy,
    R: PreTradeRisk,
    G: ExecutionGateway,
{
    pub fn new(feed: F, bus: B, book: Bk, strategy: S, risk: R, gateway: G) -> Self {
        Self {
            feed,
            bus,
            book,
            strategy,
            risk,
            gateway,
            pending: None,
        }
    }

    /// 处理单个 tick 的完整链路（内联热点，`#[inline]` 助单态化消除调用）。
    #[inline]
    fn on_tick(&mut self, ev: MarketTick, stats: &mut PipelineStats) {
        stats.ticks += 1;
        self.book.apply(&ev);
        let bid = self.book.best_bid();
        let ask = self.book.best_ask();
        let mid = self.book.mid_price();
        // 报价同步给网关（SimulatedExchange 据此刷新撮合盘口；FIX 网关忽略）。
        self.gateway.on_quote(ev.symbol_id, bid, ask);
        if let Some(mut intent) = self.strategy.on_tick(bid, ask, mid) {
            stats.intents += 1;
            intent.ts_ns = now_ns(); // 管线统一打戳，供风控做速率统计
            let mut tracked = TrackedOrder::new(intent.client_order_id);
            match self.risk.check_order(&intent) {
                Ok(()) => {
                    // Created → PendingNew
                    if tracked.advance(OrderState::PendingNew).is_err() {
                        stats.illegal_transitions += 1;
                    }
                    let ack = self.gateway.send_order(&intent);
                    stats.sent += 1;
                    let next = match ack.status {
                        AckStatus::Accepted => OrderState::Accepted,
                        AckStatus::Filled => {
                            stats.fills += 1;
                            OrderState::Filled
                        }
                        AckStatus::PartiallyFilled => {
                            stats.fills += 1;
                            OrderState::PartiallyFilled
                        }
                        AckStatus::Rejected => {
                            stats.rejected_by_gateway += 1;
                            OrderState::Rejected
                        }
                    };
                    // PendingNew → {Accepted, Filled, PartiallyFilled, Rejected}
                    if tracked.advance(next).is_err() {
                        stats.illegal_transitions += 1;
                    }
                    // 同步管线中订单在一个 tick 内走完可观测生命周期；
                    // Accepted 在此为观察终点（实盘中后续由成交回报继续推进）。
                    debug_assert_eq!(tracked.state(), next);
                }
                Err(_reason) => {
                    stats.rejected_by_risk += 1;
                    // Created → Rejected
                    if tracked.advance(OrderState::Rejected).is_err() {
                        stats.illegal_transitions += 1;
                    }
                }
            }
        }
    }

    /// 把 feed 泵入总线、再把总线排空走完链路；返回本轮是否有进展。
    ///
    /// 背压不变量：从 feed 取出的事件要么进入 bus，要么暂存在 `pending`
    /// 等待下轮，**永不静默丢弃**。
    fn pump_once(&mut self, stats: &mut PipelineStats) -> bool {
        let mut progress = false;
        // 0) 上轮暂存的事件优先入总线。
        if let Some(ev) = self.pending.take() {
            if self.bus.push(ev) {
                progress = true;
            } else {
                self.pending = Some(ev);
            }
        }
        // 1) feed → bus（bus 满则把当前事件暂存，绝不丢弃）。
        if self.pending.is_none() {
            loop {
                match self.feed.next_event() {
                    Some(ev) => {
                        if self.bus.push(ev) {
                            progress = true;
                        } else {
                            self.pending = Some(ev);
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        // 2) bus → book → strategy → risk → gateway。
        while let Some(ev) = self.bus.pop() {
            progress = true;
            self.on_tick(ev, stats);
        }
        progress
    }

    /// 回测 / 文件模式：feed 耗尽且总线排空（连续 3 轮无进展）后返回统计。
    ///
    /// 实盘常驻请用 [`Pipeline::run_forever`]。
    pub fn run(&mut self) -> PipelineStats {
        let mut stats = PipelineStats::default();
        let mut idle_rounds = 0u32;
        loop {
            if self.pump_once(&mut stats) {
                idle_rounds = 0;
            } else {
                idle_rounds += 1;
                if idle_rounds >= 3 {
                    break;
                }
            }
        }
        stats
    }

    /// 实盘模式：永不返回（feed 的 `None` 只视为暂时无数据）。
    pub fn run_forever(&mut self) -> ! {
        let mut stats = PipelineStats::default();
        loop {
            self.pump_once(&mut stats);
        }
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// main：回测装配 + 编译期换积木演示
// ---------------------------------------------------------------------------

/// 示例数据（相对本 crate 目录），`concat!` 保证与 CWD 无关。
const SAMPLE_CSV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../legos-feed/data/sample_ticks.csv");

fn main() {
    // ---- CPU 亲和性：把主循环绑到第一个可用核心，避免跨核迁移抖动 ----
    match core_affinity::get_core_ids() {
        Some(cores) if !cores.is_empty() => {
            let core = cores[0];
            if core_affinity::set_for_current(core) {
                eprintln!("[legos-app] 主循环已绑定到 CPU 核心 {}", core.id);
            } else {
                eprintln!("[legos-app] CPU 亲和性设置失败，继续运行");
            }
        }
        _ => eprintln!("[legos-app] 未检测到可用核心列表，跳过亲和性绑定"),
    }

    // ---- 回测装配：CsvFileFeed + SpscRingBuffer + L2FlatArrayBook<10>
    // ----            + MarketMakerStrategy + PassThroughRisk + SimulatedExchange
    // 第一个 CLI 参数可覆盖 CSV 路径（0 成本验证：python3 scripts/fetch_ticks.py 抓的真实数据）。
    let csv_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| SAMPLE_CSV.to_string());
    eprintln!("[legos-app] 回测数据源: {csv_path}");
    let feed = CsvFileFeed::open(&csv_path).expect("CSV 缺失");
    let bus = SpscRingBuffer::<MarketTick, 4096>::new();
    let book = L2FlatArrayBook::<10>::new();
    let strategy = MarketMakerStrategy::new(1, 200, 0, 100);
    let risk = PassThroughRisk;
    let gateway = SimulatedExchange::new(1);

    let mut pipe = Pipeline::new(feed, bus, book, strategy, risk, gateway);
    let stats = pipe.run();
    println!("[legos-app] 回测完成: {stats:#?}");
}

/// 编译期乐高替换演示：**只改泛型参数**，整条链路换成
/// `L3MapBook` + `HardLimitRisk` + `FixProtocolGateway`。
///
/// 本函数不被 main 调用，但会被编译——证明“换积木”不需要改任何逻辑代码。
/// （FIX 网关连不上 127.0.0.1:9 时返回 Rejected 而非 panic，见 legos-gateway 单测。）
#[allow(dead_code)]
fn _demo_swap_bricks_at_compile_time() {
    let feed = CsvFileFeed::open(SAMPLE_CSV).expect("示例 CSV 缺失");
    let bus = SpscRingBuffer::<MarketTick, 4096>::new();
    let book = L3MapBook::new(); // ← 把 L2FlatArrayBook<10> 换成 L3MapBook
    let strategy = MarketMakerStrategy::new(1, 200, 0, 100);
    let risk = HardLimitRisk::new(10_000_000_000, 10_000, 1_000); // ← 换成实盘风控
    let gateway = FixProtocolGateway::new("127.0.0.1:9", "LEGOS", "EXCH"); // ← 换成实盘网关

    let mut pipe: Pipeline<
        CsvFileFeed,
        SpscRingBuffer<MarketTick, 4096>,
        L3MapBook,
        MarketMakerStrategy,
        HardLimitRisk,
        FixProtocolGateway,
    > = Pipeline::new(feed, bus, book, strategy, risk, gateway);
    let stats = pipe.run();
    println!("[demo] 实盘积木装配回测: {stats:#?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use legos_core::{EventKind, OrderIntent, Side};

    /// 内存 feed：测试/基准用的确定性行情源。
    pub struct VecFeed {
        ticks: Vec<MarketTick>,
        pos: usize,
    }

    impl VecFeed {
        pub fn new(ticks: Vec<MarketTick>) -> Self {
            Self { ticks, pos: 0 }
        }
    }

    impl MarketDataFeed for VecFeed {
        fn next_event(&mut self) -> Option<MarketTick> {
            let t = *self.ticks.get(self.pos)?;
            self.pos += 1;
            Some(t)
        }
    }

    fn sample_ticks(n: usize) -> Vec<MarketTick> {
        (0..n as u32)
            .map(|i| {
                MarketTick::new(
                    1,
                    100_0000 + (i % 20) as i64 * 100,
                    10,
                    if i % 2 == 0 { Side::Bid } else { Side::Ask },
                    EventKind::Add,
                    i,
                    i as u64,
                )
            })
            .collect()
    }

    /// Mock 风控：记录 `check_order` 调用次数，可选全拒。
    ///
    /// Spec-Based Testing：验证管线对 `PreTradeRisk` trait 的调用契约——
    /// 每个 intent 恰好调用一次 `check_order`，且拒绝时订单不送网关。
    struct CountingRisk {
        calls: u64,
        reject_all: bool,
    }

    impl PreTradeRisk for CountingRisk {
        fn check_order(&mut self, _order: &OrderIntent) -> Result<(), &'static str> {
            self.calls += 1;
            if self.reject_all {
                Err(legos_core::REJECT_QTY_EXCEEDED)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn pipeline_end_to_end_backtest() {
        let feed = VecFeed::new(sample_ticks(200));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 1024>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            PassThroughRisk,
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(stats.ticks, 200, "200 个 tick 全部走完链路");
        assert!(stats.intents > 0, "做市策略应产生交易意图");
        assert!(stats.fills > 0, "模拟撮合应产生 Fill（on_quote 持续刷新盘口）");
        assert_eq!(stats.rejected_by_risk, 0);
        assert_eq!(
            stats.illegal_transitions, 0,
            "状态机非法转换必须为 0"
        );
    }

    #[test]
    fn risk_called_exactly_once_per_intent() {
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 1024>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            CountingRisk {
                calls: 0,
                reject_all: false,
            },
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(pipe.risk.calls, stats.intents, "每个 intent 恰好一次 check_order");
        assert!(stats.intents > 0);
        assert_eq!(stats.illegal_transitions, 0);
    }

    #[test]
    fn risk_rejection_short_circuits_gateway() {
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 1024>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            CountingRisk {
                calls: 0,
                reject_all: true,
            },
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(stats.rejected_by_risk, stats.intents);
        assert_eq!(stats.sent, 0, "被风控拦截的订单不得送网关");
        assert_eq!(pipe.gateway.fills, 0);
        assert_eq!(stats.illegal_transitions, 0);
    }

    #[test]
    fn no_tick_loss_when_bus_fills() {
        // bus 容量 8、feed 5000 个 tick：旧实现在 bus 满时静默丢弃事件。
        // 背压不变量：run() 结束时 pending 为空，且 ticks == feed 总数。
        let feed = VecFeed::new(sample_ticks(5000));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 8>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            PassThroughRisk,
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(stats.ticks, 5000, "bus 背压下也不得丢 tick");
        assert!(pipe.pending.is_none(), "结束时暂存槽必须为空");
        assert_eq!(stats.illegal_transitions, 0);
    }

    #[test]
    fn backpressure_stress_producer_faster_than_consumer() {
        // 背压满载压测（P4）：生产速度 > 消费速度持续一段时间。
        //
        // 场景：feed 一次性给出 200_000 个 tick，bus 容量仅 16。
        // 每轮 `pump_once` 的 feed→bus 内循环都会把 bus 灌满（push 失败），
        // 当前事件暂存进 `pending` 槽，下轮优先入总线——`pending` 被反复使用。
        // 断言：零丢失（ticks == feed 总数）、结束时 pending 为空；
        // 测量：drain 总耗时、单轮最大停顿（max round stall）、吞吐。
        const N: usize = 200_000;
        let feed = VecFeed::new(sample_ticks(N));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 16>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            PassThroughRisk,
            SimulatedExchange::new(1),
        );
        let mut stats = PipelineStats::default();
        let mut rounds: u64 = 0;
        let mut pending_rounds: u64 = 0;
        let mut max_round_ns: u64 = 0;
        let t0 = std::time::Instant::now();
        // 与 `run()` 相同的 3 轮无进展退出语义。
        let mut idle_rounds = 0u32;
        loop {
            let r0 = std::time::Instant::now();
            let progress = pipe.pump_once(&mut stats);
            let dt = r0.elapsed().as_nanos() as u64;
            if dt > max_round_ns {
                max_round_ns = dt;
            }
            rounds += 1;
            if pipe.pending.is_some() {
                pending_rounds += 1;
            }
            if progress {
                idle_rounds = 0;
            } else {
                idle_rounds += 1;
                if idle_rounds >= 3 {
                    break;
                }
            }
        }
        let total = t0.elapsed();
        assert!(
            pending_rounds > 0,
            "背压必须真实发生：pending 槽应被实际使用"
        );
        assert_eq!(stats.ticks, N as u64, "bus 背压下永不丢 tick");
        assert!(pipe.pending.is_none(), "结束时暂存槽必须为空");
        assert_eq!(stats.illegal_transitions, 0);
        eprintln!(
            "[backpressure] N={N} rounds={rounds} pending_rounds={pending_rounds} \
             drain_total={total:?} max_round_stall={max_round_ns}ns \
             throughput={:.0} ticks/s",
            N as f64 / total.as_secs_f64(),
        );
    }

    #[test]
    fn pipeline_with_hard_risk_blocks_oversize() {
        // 风控上限设得很低：大名义金额意图应被拦截。
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 1024>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            HardLimitRisk::new(1, 10, 1_000_000), // max_notional=1，几乎全拒
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert!(stats.rejected_by_risk > 0);
        assert_eq!(stats.sent, 0);
        assert_eq!(stats.illegal_transitions, 0);
    }

    #[test]
    fn pipeline_with_l3_book_compiles_and_runs() {
        // 换账簿积木：同一套 run() 逻辑直接复用。
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<MarketTick, 1024>::new(),
            L3MapBook::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            PassThroughRisk,
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(stats.ticks, 50);
        assert_eq!(stats.illegal_transitions, 0);
    }
}

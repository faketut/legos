//! legos-app: 把六大积木在**编译期**织成一条直线流水线的二进制。
//!
//! ```text
//! feed.next_event() → bus.push() → bus.pop() → book.apply()
//!     → strategy.on_tick() → risk.check() → gateway.send_order()
//! ```
//!
//! `Pipeline` 的 6 个类型参数全部是**泛型**（不是 `dyn`）：
//! `cargo build` 做单态化时把每一层调用直接内联，运行时不存在任何
//! 虚函数跳转——这就是「Legos 编译期积木」名称的由来。

use std::time::{SystemTime, UNIX_EPOCH};

use legos_book::{L2FlatArrayBook, L3MapBook};
use legos_bus::SpscRingBuffer;
use legos_core::{
    AckStatus, ExecutionGateway, MarketDataFeed, MessageBus, OrderBook, PreTradeRisk, Tick,
    TradingStrategy,
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
}

impl<F, B, Bk, S, R, G> Pipeline<F, B, Bk, S, R, G>
where
    F: MarketDataFeed,
    B: MessageBus<Item = Tick>,
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
        }
    }

    /// 处理单个 tick 的完整链路（内联热点，`#[inline]` 助单态化消除调用）。
    #[inline]
    fn on_tick(&mut self, ev: Tick, stats: &mut PipelineStats) {
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
            match self.risk.check(&intent) {
                Ok(()) => {
                    let ack = self.gateway.send_order(&intent);
                    stats.sent += 1;
                    match ack.status {
                        AckStatus::Filled | AckStatus::PartiallyFilled => stats.fills += 1,
                        AckStatus::Rejected => stats.rejected_by_gateway += 1,
                        AckStatus::Accepted => {}
                    }
                }
                Err(_) => stats.rejected_by_risk += 1,
            }
        }
    }

    /// 把 feed 泵入总线、再把总线排空走完链路；返回本轮是否有进展。
    fn pump_once(&mut self, stats: &mut PipelineStats) -> bool {
        let mut progress = false;
        // 1) feed → bus（bus 满则停下先去 drain，避免覆盖）。
        loop {
            match self.feed.next_event() {
                Some(ev) => {
                    if self.bus.push(ev) {
                        progress = true;
                    } else {
                        break;
                    }
                }
                None => break,
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
    let feed = CsvFileFeed::open(SAMPLE_CSV).expect("示例 CSV 缺失");
    let bus = SpscRingBuffer::<Tick, 4096>::new();
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
    let bus = SpscRingBuffer::<Tick, 4096>::new();
    let book = L3MapBook::new(); // ← 把 L2FlatArrayBook<10> 换成 L3MapBook
    let strategy = MarketMakerStrategy::new(1, 200, 0, 100);
    let risk = HardLimitRisk::new(10_000_000_000, 10_000, 1_000); // ← 换成实盘风控
    let gateway = FixProtocolGateway::new("127.0.0.1:9", "LEGOS", "EXCH"); // ← 换成实盘网关

    let mut pipe: Pipeline<
        CsvFileFeed,
        SpscRingBuffer<Tick, 4096>,
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
    use legos_core::{EventKind, Side};

    /// 内存 feed：测试/基准用的确定性行情源。
    pub struct VecFeed {
        ticks: Vec<Tick>,
        pos: usize,
    }

    impl VecFeed {
        pub fn new(ticks: Vec<Tick>) -> Self {
            Self { ticks, pos: 0 }
        }
    }

    impl MarketDataFeed for VecFeed {
        fn next_event(&mut self) -> Option<Tick> {
            let t = *self.ticks.get(self.pos)?;
            self.pos += 1;
            Some(t)
        }
    }

    fn sample_ticks(n: usize) -> Vec<Tick> {
        (0..n as u64)
            .map(|i| Tick {
                symbol_id: 1,
                price: 100_0000 + (i % 20) as i64 * 100,
                qty: 10,
                side: if i % 2 == 0 { Side::Bid } else { Side::Ask },
                kind: EventKind::Add,
                order_id: i,
                ts_ns: i,
            })
            .collect()
    }

    #[test]
    fn pipeline_end_to_end_backtest() {
        let feed = VecFeed::new(sample_ticks(200));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<Tick, 1024>::new(),
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
    }

    #[test]
    fn pipeline_with_hard_risk_blocks_oversize() {
        // 风控上限设得很低：大名义金额意图应被拦截。
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<Tick, 1024>::new(),
            L2FlatArrayBook::<10>::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            HardLimitRisk::new(1, 10, 1_000_000), // max_notional=1，几乎全拒
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert!(stats.rejected_by_risk > 0);
        assert_eq!(stats.sent, 0);
    }

    #[test]
    fn pipeline_with_l3_book_compiles_and_runs() {
        // 换账簿积木：同一套 run() 逻辑直接复用。
        let feed = VecFeed::new(sample_ticks(50));
        let mut pipe = Pipeline::new(
            feed,
            SpscRingBuffer::<Tick, 1024>::new(),
            L3MapBook::new(),
            MarketMakerStrategy::new(1, 200, 0, 10),
            PassThroughRisk,
            SimulatedExchange::new(1),
        );
        let stats = pipe.run();
        assert_eq!(stats.ticks, 50);
    }
}

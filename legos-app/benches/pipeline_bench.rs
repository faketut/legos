//! 全链路延迟基准：feed → bus → book → strategy → risk → gateway。
//!
//! 运行：`cargo bench -p legos-app`（release 模式，结果见 target/criterion）。
//!
//! **基准契约**（见 `legos-app/SPEC.md`）：
//!
//! * 每次 `b.iter` 内重建整条管线，测量 1000 tick 端到端（含策略下单、
//!   风控、模拟撮合），不得复用上一次迭代的账簿/网关状态；
//! * `black_box` 包裹输入输出，禁止编译器把链路优化为空；
//! * 断言 fills > 0：基准测的是真实成交路径，不是空转。

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use legos_book::L2FlatArrayBook;
use legos_bus::SpscRingBuffer;
use legos_core::{
    AckStatus, EventKind, ExecutionGateway, MarketDataFeed, MarketTick, OrderBook, PreTradeRisk,
    Side, TradingStrategy,
};
use legos_gateway::SimulatedExchange;
use legos_risk::PassThroughRisk;
use legos_strategy::MarketMakerStrategy;

// 基准需要复用管线类型，但 `Pipeline` 在 binary crate 里。
// 为避免把 binary 拆成 lib，基准内联一份最小装配（与 main.rs 逻辑一致）。
struct BenchPipeline {
    feed: VecFeed,
    bus: SpscRingBuffer<MarketTick, 4096>,
    book: L2FlatArrayBook<10>,
    strategy: MarketMakerStrategy,
    risk: PassThroughRisk,
    gateway: SimulatedExchange,
}

pub struct VecFeed {
    ticks: Vec<MarketTick>,
    pos: usize,
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
                i as u64 * 1000,
            )
        })
        .collect()
}

fn bench_spsc(c: &mut Criterion) {
    let mut g = c.benchmark_group("spsc_ring_buffer");
    g.throughput(Throughput::Elements(1));
    g.bench_function("push_pop_tick", |b| {
        let q = SpscRingBuffer::<MarketTick, 4096>::new();
        let t = sample_ticks(1)[0];
        b.iter(|| {
            black_box(q.push(black_box(t)));
            black_box(q.pop());
        })
    });
    g.finish();
}

fn bench_l2_book(c: &mut Criterion) {
    let ticks = sample_ticks(10_000);
    let mut g = c.benchmark_group("l2_book");
    g.throughput(Throughput::Elements(1));
    g.bench_function("apply_tick", |b| {
        let mut book = L2FlatArrayBook::<10>::new();
        let mut i = 0;
        b.iter(|| {
            book.apply(black_box(&ticks[i % ticks.len()]));
            i += 1;
        })
    });
    g.finish();
}

fn bench_full_pipeline(c: &mut Criterion) {
    let mut g = c.benchmark_group("full_pipeline");
    g.throughput(Throughput::Elements(1000));
    g.bench_function("tick_to_fill_1000", |b| {
        b.iter(|| {
            let mut p = BenchPipeline {
                feed: VecFeed {
                    ticks: sample_ticks(1000),
                    pos: 0,
                },
                bus: SpscRingBuffer::<MarketTick, 4096>::new(),
                book: L2FlatArrayBook::<10>::new(),
                strategy: MarketMakerStrategy::new(1, 200, 0, 10),
                risk: PassThroughRisk,
                gateway: SimulatedExchange::new(1),
            };
            let mut fills = 0u64;
            while let Some(ev) = p.feed.next_event() {
                p.bus.push(ev);
                while let Some(e) = p.bus.pop() {
                    p.book.apply(&e);
                    let (bid, ask, mid) =
                        (p.book.best_bid(), p.book.best_ask(), p.book.mid_price());
                    p.gateway.on_quote(e.symbol_id, bid, ask);
                    if let Some(intent) = p.strategy.on_tick(bid, ask, mid) {
                        if p.risk.check_order(&intent).is_ok() {
                            let ack = p.gateway.send_order(&intent);
                            if matches!(
                                ack.status,
                                AckStatus::Filled | AckStatus::PartiallyFilled
                            ) {
                                fills += 1;
                            }
                        }
                    }
                }
            }
            // 契约：基准必须走真实成交路径
            assert!(fills > 0, "基准退化：没有产生任何成交");
            black_box(fills)
        })
    });
    g.finish();
}

criterion_group!(benches, bench_spsc, bench_l2_book, bench_full_pipeline);
criterion_main!(benches);

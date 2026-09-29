//! 全链路延迟基准：feed → bus → book → strategy → risk → gateway。
//!
//! 运行：`cargo bench -p legos-app`（release 模式，结果见 target/criterion）。

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use legos_book::L2FlatArrayBook;
use legos_bus::SpscRingBuffer;
use legos_core::{
    AckStatus, EventKind, ExecutionGateway, MarketDataFeed, OrderBook, PreTradeRisk, Side, Tick,
    TradingStrategy,
};
use legos_gateway::SimulatedExchange;
use legos_risk::PassThroughRisk;
use legos_strategy::MarketMakerStrategy;

// 基准需要复用管线类型，但 `Pipeline` 在 binary crate 里。
// 为避免把 binary 拆成 lib，基准内联一份最小装配（与 main.rs 逻辑一致）。
struct BenchPipeline {
    feed: VecFeed,
    bus: SpscRingBuffer<Tick, 4096>,
    book: L2FlatArrayBook<10>,
    strategy: MarketMakerStrategy,
    risk: PassThroughRisk,
    gateway: SimulatedExchange,
}

pub struct VecFeed {
    ticks: Vec<Tick>,
    pos: usize,
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
            ts_ns: i * 1000,
        })
        .collect()
}

fn bench_spsc(c: &mut Criterion) {
    let mut g = c.benchmark_group("spsc_ring_buffer");
    g.throughput(Throughput::Elements(1));
    g.bench_function("push_pop_tick", |b| {
        let q = SpscRingBuffer::<Tick, 4096>::new();
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
                bus: SpscRingBuffer::<Tick, 4096>::new(),
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
                        if p.risk.check(&intent).is_ok() {
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
            black_box(fills)
        })
    });
    g.finish();
}

criterion_group!(benches, bench_spsc, bench_l2_book, bench_full_pipeline);
criterion_main!(benches);

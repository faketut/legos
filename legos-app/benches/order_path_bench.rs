//! 下单路径单独压测：strategy → risk → gateway（SimulatedExchange 撮合）。
//!
//! 背景：`pipeline_bench::full_pipeline` 均值 ≈ bus + book
//! （20.7 + 21.4 ≈ 42ns/tick），下单路径被平均数稀释。
//! 本基准强制**每 tick 产生一笔可成交订单**，隔离测量下单路径延迟。
//!
//! 运行：`cargo bench -p legos-app --bench order_path_bench`（release）。
//!
//! **基准契约**：
//!
//! * 每 tick 必产生订单：测试策略 [`AlwaysMarketStrategy`] 发市价单
//!  （`price == 0` → 按 sim 语义恒可成交）；
//! * 每 tick 先 `gateway.on_quote` 刷新撮合盘口（与真实管线一致，
//!   见 `legos-app/SPEC.md` §1.1），再走
//!   `strategy.on_tick → risk.check_order → gateway.send_order`；
//!   主分位数口径的计时区间为后三者（纯下单路径），
//!   `on_quote` 成本见独立的 `gateway_on_quote` 拆分基准；
//! * 计时方法学与 `tail_latency` 一致：x86 `rdtsc` 逐单计时，
//!   扣除背对背 `rdtsc` 本底（`saturating_sub`），样本写入预分配数组，
//!   事后离线排序算分位数；测量线程尽力绑核；
//! * `black_box` 包裹回执；断言 `fills == N`——测的是真实成交路径，不是空转。

use std::arch::x86_64::_rdtsc;
use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use legos_core::{
    AckStatus, ExecutionGateway, OrderIntent, PreTradeRisk, Side, TradingStrategy,
};
use legos_gateway::SimulatedExchange;
use legos_risk::PassThroughRisk;
use legos_strategy::MarketMakerStrategy;

/// 每 tick 必下单的测试策略：轮流发出买卖**市价单**
/// （`price == 0`，按 `SimulatedExchange` 语义恒可成交）。
struct AlwaysMarketStrategy {
    next_id: u64,
    buy_next: bool,
}

impl AlwaysMarketStrategy {
    fn new() -> Self {
        Self {
            next_id: 0,
            buy_next: true,
        }
    }
}

impl TradingStrategy for AlwaysMarketStrategy {
    fn on_tick(
        &mut self,
        _bid: Option<(i64, u32)>,
        _ask: Option<(i64, u32)>,
        _mid: Option<i64>,
    ) -> Option<OrderIntent> {
        self.next_id += 1;
        let side = if self.buy_next {
            Side::Bid
        } else {
            Side::Ask
        };
        self.buy_next = !self.buy_next;
        Some(OrderIntent {
            client_order_id: self.next_id,
            symbol_id: 1,
            side,
            price: 0, // 市价单
            qty: 10,
            ts_ns: 0,
        })
    }
}

#[inline(always)]
fn rdtsc() -> u64 {
    // SAFETY: x86_64 上 rdtsc 始终可用；VM 中为虚拟化计数器，单调即可用于差值计时。
    unsafe { _rdtsc() }
}

/// 睡眠标定 TSC 频率（Hz）。
fn calibrate_tsc_hz() -> f64 {
    let t0 = rdtsc();
    let wall0 = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    let wall = wall0.elapsed().as_secs_f64();
    let t1 = rdtsc();
    (t1 - t0) as f64 / wall
}

/// 背对背 rdtsc 最小差值 = 计时器本底开销（cycles）。
fn rdtsc_overhead_floor() -> u64 {
    let mut floor = u64::MAX;
    for _ in 0..10_000 {
        black_box(rdtsc());
    }
    for _ in 0..1_000_000 {
        let a = rdtsc();
        let b = rdtsc();
        let d = b.wrapping_sub(a);
        if d < floor {
            floor = d;
        }
    }
    floor
}

struct Percentiles {
    mean_ns: f64,
    p50_ns: f64,
    p99_ns: f64,
    max_ns: f64,
    fills: u64,
}

/// 下单路径逐单分位数测量（N=200k 样本）。
///
/// 计时区间：`strategy.on_tick → risk.check_order → gateway.send_order`。
/// 每样本前用 `on_quote` 刷新盘口（对手盘各 10_000 量，保证市价单全额成交），
/// 该刷新不计入计时（见 `gateway_on_quote` 拆分基准）。
fn measure_order_path(n: usize) -> Percentiles {
    // 尽力绑核，减少调度噪声；失败不中断（与 legos-app/SPEC.md §1.3 一致）。
    if let Some(cores) = core_affinity::get_core_ids() {
        if let Some(core) = cores.first() {
            let _ = core_affinity::set_for_current(*core);
        }
    }
    let tsc_hz = calibrate_tsc_hz();
    let floor = rdtsc_overhead_floor();

    let mut strategy = AlwaysMarketStrategy::new();
    let mut risk = PassThroughRisk;
    let mut gateway = SimulatedExchange::new(1);

    // 预热：分支预测 / 指令缓存进入稳态。
    for i in 0..20_000u64 {
        let mid = 1_000_000 + (i % 64) as i64 * 100;
        gateway.on_quote(1, Some((mid - 10, 10_000)), Some((mid + 10, 10_000)));
        let intent = strategy.on_tick(None, None, Some(mid)).unwrap();
        let _ = risk.check_order(&intent);
        black_box(gateway.send_order(&intent));
    }

    let mut samples = Vec::with_capacity(n);
    let mut fills = 0u64;
    for i in 0..n as u64 {
        let mid = 1_000_000 + (i % 64) as i64 * 100;
        // 盘口刷新：真实管线每 tick 都做；不计入下单路径计时。
        gateway.on_quote(1, Some((mid - 10, 10_000)), Some((mid + 10, 10_000)));

        let t0 = rdtsc();
        let intent = strategy.on_tick(None, None, Some(mid)).unwrap();
        risk.check_order(&intent).expect("测试策略订单应被风控放行");
        let ack = gateway.send_order(&intent);
        let t1 = rdtsc();

        samples.push(t1.wrapping_sub(t0).saturating_sub(floor));
        if matches!(ack.status, AckStatus::Filled) {
            fills += 1;
        }
        black_box(ack);
    }
    // 契约：每单都应全额成交，否则测的不是成交路径。
    assert_eq!(fills, n as u64, "下单路径基准退化：有订单未成交");

    samples.sort_unstable();
    let c2ns = 1e9 / tsc_hz;
    let sum: u128 = samples.iter().map(|&x| x as u128).sum();
    Percentiles {
        mean_ns: sum as f64 / n as f64 * c2ns,
        p50_ns: samples[n / 2] as f64 * c2ns,
        p99_ns: samples[n * 99 / 100] as f64 * c2ns,
        max_ns: samples[n - 1] as f64 * c2ns,
        fills,
    }
}

fn bench_order_path_percentiles(c: &mut Criterion) {
    let mut g = c.benchmark_group("order_path");
    g.bench_function("per_order_p50_p99_manual", |b| {
        b.iter(|| {
            let p = measure_order_path(200_000);
            println!(
                "[order_path] N=200000 fills={} | mean={:.1}ns p50={:.1}ns p99={:.1}ns max={:.1}ns",
                p.fills, p.mean_ns, p.p50_ns, p.p99_ns, p.max_ns
            );
            black_box(p.mean_ns);
        })
    });
    g.finish();
}

/// criterion 口径的整条下单路径均值（与手动分位数互相印证）。
fn bench_order_path_criterion(c: &mut Criterion) {
    let mut g = c.benchmark_group("order_path");
    g.throughput(Throughput::Elements(1));
    g.bench_function("per_order_criterion_mean", |b| {
        let mut strategy = AlwaysMarketStrategy::new();
        let mut risk = PassThroughRisk;
        let mut gateway = SimulatedExchange::new(1);
        let mut i = 0u64;
        b.iter(|| {
            let mid = 1_000_000 + (i % 64) as i64 * 100;
            i += 1;
            gateway.on_quote(1, Some((mid - 10, 10_000)), Some((mid + 10, 10_000)));
            let intent = strategy.on_tick(None, None, Some(mid)).unwrap();
            let _ = risk.check_order(&intent);
            black_box(gateway.send_order(black_box(&intent)));
        })
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 拆分基准：定位下单路径内部热点（strategy / risk / gateway 各自成本）
// ---------------------------------------------------------------------------

fn bench_strategy_on_tick(c: &mut Criterion) {
    let mut g = c.benchmark_group("order_path_split");
    g.throughput(Throughput::Elements(1));
    g.bench_function("strategy_on_tick_mm", |b| {
        let mut s = MarketMakerStrategy::new(1, 200, 0, 10);
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let mid = 1_000_000 + (i % 64) as i64 * 100;
            black_box(s.on_tick(
                black_box(Some((mid - 100, 10_000))),
                black_box(Some((mid + 100, 10_000))),
                black_box(Some(mid)),
            ));
        })
    });
    g.finish();
}

fn bench_risk_check(c: &mut Criterion) {
    let mut g = c.benchmark_group("order_path_split");
    g.throughput(Throughput::Elements(1));
    g.bench_function("risk_check_passthrough", |b| {
        let mut risk = PassThroughRisk;
        let intent = OrderIntent {
            client_order_id: 1,
            symbol_id: 1,
            side: Side::Bid,
            price: 0,
            qty: 10,
            ts_ns: 0,
        };
        b.iter(|| {
            black_box(risk.check_order(black_box(&intent)));
        })
    });
    g.finish();
}

fn bench_gateway_send_order(c: &mut Criterion) {
    let mut g = c.benchmark_group("order_path_split");
    g.throughput(Throughput::Elements(1));
    // 成交路径：市价单 + 对手盘巨量（永不耗尽），覆盖查报价→撮合→扣减全流程。
    g.bench_function("gateway_send_order_fill", |b| {
        let mut gateway = SimulatedExchange::new(1);
        gateway.update_quote(1, (999_990, u32::MAX / 2), (1_000_010, u32::MAX / 2));
        let mut id = 0u64;
        b.iter(|| {
            id += 1;
            let intent = OrderIntent {
                client_order_id: id,
                symbol_id: 1,
                side: Side::Bid,
                price: 0,
                qty: 10,
                ts_ns: 0,
            };
            black_box(gateway.send_order(black_box(&intent)));
        })
    });
    // 盘口刷新路径：真实管线每 tick 调用一次 on_quote。
    g.bench_function("gateway_on_quote", |b| {
        let mut gateway = SimulatedExchange::new(1);
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let mid = 1_000_000 + (i % 64) as i64 * 100;
            gateway.on_quote(
                black_box(1),
                black_box(Some((mid - 10, 10_000))),
                black_box(Some((mid + 10, 10_000))),
            );
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_order_path_percentiles,
    bench_order_path_criterion,
    bench_strategy_on_tick,
    bench_risk_check,
    bench_gateway_send_order,
);
criterion_main!(benches);

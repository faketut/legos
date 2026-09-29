//! Tail-latency 独立测量 harness（不走 criterion 统计，直接输出分位数）。
//!
//! 运行：`cargo run --release -p legos-app --bin tail_latency -- [N_TICKS]`
//! 默认 10_000_000 tick，可通过参数调整。
//!
//! # 方法学（诚实声明）
//!
//! * 逐 tick 用 x86 `rdtsc` 计时（约 20~30 cycles 本底，远小于
//!   `Instant::now()` 的 ~20ns 系统调用开销），样本写入预分配数组，
//!   事后一次性算分位数；
//! * TSC 频率用 200ms 睡眠标定（`calibrate_tsc_hz`），cycles → ns 据此换算；
//! * 计时器本底用 1M 次背对背 `rdtsc` 的最小差值标定，每个样本做
//!   `saturating_sub` 扣除，并在输出中如实报告本底值；
//! * 测量线程绑定到一个 CPU 核心（`core_affinity`），减少调度噪声；
//! * 10M tick 预先生成（确定性合成数据，与 `pipeline_bench.rs` 同分布），
//!   测量前 200k tick 预热；
//! * 测的是**进程内计算**：feed → bus → book → strategy → risk → gateway，
//!   不含网络 I/O；与行业 wire-to-wire 数字对比时必须注明口径差异。
//!
//! 断言 fills > 0：保证测的是真实成交路径，不是空转。

use std::arch::x86_64::_rdtsc;
use std::hint::black_box;
use std::time::{Duration, Instant};

use legos_book::L2FlatArrayBook;
use legos_bus::SpscRingBuffer;
use legos_core::{
    AckStatus, EventKind, ExecutionGateway, MarketDataFeed, MarketTick, OrderBook, PreTradeRisk,
    Side, TradingStrategy,
};
use legos_gateway::SimulatedExchange;
use legos_risk::PassThroughRisk;
use legos_strategy::MarketMakerStrategy;

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
    // 先让分支预测器/CPU 稳定
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

pub struct VecFeed {
    ticks: Vec<MarketTick>,
    pos: usize,
}

impl MarketDataFeed for VecFeed {
    #[inline(always)]
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

#[inline(never)]
fn pump_one(
    feed: &mut VecFeed,
    bus: &SpscRingBuffer<MarketTick, 4096>,
    book: &mut L2FlatArrayBook<10>,
    strategy: &mut MarketMakerStrategy,
    risk: &mut PassThroughRisk,
    gateway: &mut SimulatedExchange,
) -> u64 {
    // 返回本 tick 产生的 fill 数；与 pipeline_bench.rs / main.rs 逻辑一致。
    let mut fills = 0u64;
    if let Some(ev) = feed.next_event() {
        let ev = black_box(ev);
        bus.push(ev);
        while let Some(e) = bus.pop() {
            book.apply(&e);
            let (bid, ask, mid) = (book.best_bid(), book.best_ask(), book.mid_price());
            gateway.on_quote(e.symbol_id, bid, ask);
            if let Some(intent) = strategy.on_tick(bid, ask, mid) {
                if risk.check_order(&intent).is_ok() {
                    let ack = gateway.send_order(&intent);
                    if matches!(ack.status, AckStatus::Filled | AckStatus::PartiallyFilled) {
                        fills += 1;
                    }
                }
            }
        }
    }
    fills
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    let idx = ((q / 100.0) * (sorted.len() - 1) as f64) as usize;
    sorted[idx]
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);
    println!("tail_latency: N = {n} ticks");

    // 绑定 CPU 核心，减少调度噪声。
    match core_affinity::get_core_ids() {
        Some(ids) if ids.len() > 1 => {
            let core = ids[1 % ids.len()];
            if core_affinity::set_for_current(core) {
                println!("pinned to core {}", core.id);
            }
        }
        _ => println!("core_affinity unavailable, no pinning"),
    }

    let hz = calibrate_tsc_hz();
    println!("tsc_hz = {hz:.3e} ticks/s (200ms sleep calibration)");
    let floor = rdtsc_overhead_floor();
    println!("rdtsc overhead floor = {floor} cycles (subtracted from every sample)");

    let ticks = sample_ticks(n);
    let mut feed = VecFeed { ticks, pos: 0 };
    let bus = SpscRingBuffer::<MarketTick, 4096>::new();
    let mut book = L2FlatArrayBook::<10>::new();
    let mut strategy = MarketMakerStrategy::new(1, 200, 0, 10);
    let mut risk = PassThroughRisk;
    let mut gateway = SimulatedExchange::new(1);

    // 预热：让指令缓存/分支预测器进入稳态，不计入样本。
    const WARMUP: usize = 200_000;
    let mut warm_fills = 0u64;
    let mut warm_ticks = sample_ticks(WARMUP);
    let mut warm_feed = VecFeed {
        ticks: std::mem::take(&mut warm_ticks),
        pos: 0,
    };
    let warm_bus = SpscRingBuffer::<MarketTick, 4096>::new();
    let mut warm_book = L2FlatArrayBook::<10>::new();
    let mut warm_strategy = MarketMakerStrategy::new(1, 200, 0, 10);
    let mut warm_risk = PassThroughRisk;
    let mut warm_gateway = SimulatedExchange::new(1);
    for _ in 0..WARMUP {
        warm_fills += pump_one(
            &mut warm_feed,
            &warm_bus,
            &mut warm_book,
            &mut warm_strategy,
            &mut warm_risk,
            &mut warm_gateway,
        );
    }
    black_box(warm_fills);
    println!("warmup done ({WARMUP} ticks)");

    // 主测量：逐 tick rdtsc，样本进预分配数组。
    let mut samples: Vec<u64> = Vec::with_capacity(n);
    let mut fills = 0u64;
    for _ in 0..n {
        let t0 = rdtsc();
        fills += pump_one(&mut feed, &bus, &mut book, &mut strategy, &mut risk, &mut gateway);
        let t1 = rdtsc();
        samples.push(t1.wrapping_sub(t0).saturating_sub(floor));
    }
    assert!(fills > 0, "基准退化：没有产生任何成交");
    println!("fills = {fills}");

    // 分位数：一次性排序（事后离线计算，不污染测量）。
    let t_sort = Instant::now();
    samples.sort_unstable();
    println!("sort took {:?}", t_sort.elapsed());

    let c2ns = 1e9 / hz;
    let mean_c: f64 = samples.iter().map(|&x| x as f64).sum::<f64>() / samples.len() as f64;
    let p = |q: f64| percentile(&samples, q) as f64 * c2ns;
    println!("+----------------+------------+");
    println!("| metric         | latency    |");
    println!("+----------------+------------+");
    println!("| mean           | {:>8.1} ns |", mean_c * c2ns);
    println!("| p50            | {:>8.1} ns |", p(50.0));
    println!("| p99            | {:>8.1} ns |", p(99.0));
    println!("| p999           | {:>8.1} ns |", p(99.9));
    println!("| max            | {:>8.1} ns |", p(100.0));
    println!("+----------------+------------+");
    println!("note: per-tick pipeline latency, in-process compute only (no network I/O).");
}

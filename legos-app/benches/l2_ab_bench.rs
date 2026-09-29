//! L2 A/B 基准：`L2FlatArrayBook<10>`（二分查找 + `copy_within` 平移）
//! vs `L2DirectIndexBook`（价格 → 数组下标直接映射，固定档距，确定性更新）。
//!
//! 运行（release）：
//! `cargo bench -p legos-app --bench l2_ab_bench -- --nocapture`
//! `[tail]` 开头的行是手动逐 tick 计时输出的 mean/p50/p99/p999/max。
//!
//! **判定线**（见 `legos-book/SPEC.md` §4）：O(1) 版 p999 改善 ≥10%
//! **且** p50 不回归，才把 O(1) 版设为管线默认；否则双实现保留，
//! 以泛型参数切换。

use std::sync::Once;
use std::time::{Duration, Instant};

use criterion::{
    black_box, criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, Criterion,
    Throughput,
};
use legos_book::{L2DirectIndexBook, L2FlatArrayBook};
use legos_core::{EventKind, MarketTick, OrderBook, Side};

/// O(1) 账簿的固定档距契约：BASE=99_0000，TICK=100，
/// 覆盖 99_0000..109_2400（1024 档）。
const BASE: i64 = 99_0000;
const TICK: i64 = 100;
type FlatBook = L2FlatArrayBook<10>;
type O1Book = L2DirectIndexBook<BASE, TICK>;

fn tick(side: Side, price: i64, qty: u32, kind: EventKind, seq: u32) -> MarketTick {
    MarketTick::new(1, price, qty, side, kind, seq, seq as u64)
}

/// 常规混合流：确定性 LCG 伪随机，70% Add / 15% Cancel / 15% Trade，
/// 买卖各 40 个网格档（价格 100_0000..100_3900，对齐 TICK=100）。
fn mixed_ticks(n: usize) -> Vec<MarketTick> {
    let mut rng = 0x9E37_79B9u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..n as u32)
        .map(|i| {
            let r = next();
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            let slot = 100 + (r >> 1) % 40;
            let price = BASE + slot as i64 * TICK;
            let kind = match (r >> 7) % 100 {
                0..=69 => EventKind::Add,
                70..=84 => EventKind::Cancel,
                _ => EventKind::Trade,
            };
            tick(side, price, 1 + (r >> 13) as u32 % 50, kind, i)
        })
        .collect()
}

/// 最坏序列：针对 flat 版的满数组 `memmove` 构造——
/// 先填满 10 档买盘（100_0000..100_0900），然后每轮 add 一个更优的新最优价
/// （下标 0 插入，满数组平移 9 个元素）+ 紧接着 cancel 掉它
/// （下标 0 删除，平移 9 个元素）。
/// 两个最优价（100_9500 / 100_9400）交替使用，避免价格单调漂移出
/// O(1) 账簿的覆盖区间；每次价格跳变都跨越多档（跨档跳变）。
fn worst_ticks(rounds: usize) -> Vec<MarketTick> {
    let mut v = Vec::with_capacity(10 + rounds * 2);
    for k in 0..10u32 {
        v.push(tick(Side::Bid, 100_0000 + k as i64 * 100, 10, EventKind::Add, k));
    }
    for r in 0..rounds as u32 {
        let p = if r % 2 == 0 { 100_9500 } else { 100_9400 };
        let seq = 10 + r * 2;
        v.push(tick(Side::Bid, p, 10, EventKind::Add, seq));
        v.push(tick(Side::Bid, p, 10, EventKind::Cancel, seq + 1));
    }
    v
}

/// criterion 均值对比：两种实现跑同一 tick 流，测 `apply` 吞吐。
fn bench_apply<B: OrderBook>(
    g: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    ticks: &[MarketTick],
    mut make: impl FnMut() -> B,
) {
    g.bench_function(name, |b| {
        let mut book = make();
        let mut i = 0usize;
        b.iter(|| {
            book.apply(black_box(&ticks[i % ticks.len()]));
            i += 1;
        })
    });
}

fn bench_ab_mean(c: &mut Criterion) {
    let mixed = mixed_ticks(50_000);
    let worst = worst_ticks(25_000);
    let mut g = c.benchmark_group("l2_ab_mean");
    g.throughput(Throughput::Elements(1));
    bench_apply(&mut g, "flat_mixed", &mixed, FlatBook::new);
    bench_apply(&mut g, "o1_mixed", &mixed, O1Book::new);
    bench_apply(&mut g, "flat_worst", &worst, FlatBook::new);
    bench_apply(&mut g, "o1_worst", &worst, O1Book::new);
    g.finish();
}

/// 单遍手动测量：逐 tick 用 `Instant` 计时（含一次 best_bid/best_ask/mid
/// 读取，还原管线真实路径），输出 mean/p50/p99/p999/max。
/// 返回 max，供 `black_box` 消费（`println!` 本身已阻止 DCE）。
fn report_tail<B: OrderBook>(name: &str, ticks: &[MarketTick], mut make: impl FnMut() -> B) -> u64 {
    let mut book = make();
    let mut dts = Vec::with_capacity(ticks.len());
    let mut checksum = 0u64;
    for t in ticks {
        let s = Instant::now();
        book.apply(black_box(t));
        let dt = s.elapsed().as_nanos() as u64;
        dts.push(dt);
        // 还原管线真实路径：每 tick 都读最优报价。
        if let Some((p, q)) = book.best_bid() {
            checksum = checksum.wrapping_add(p as u64).wrapping_add(q as u64);
        }
        if let Some((p, q)) = book.best_ask() {
            checksum = checksum.wrapping_add(p as u64).wrapping_add(q as u64);
        }
        black_box(book.mid_price());
    }
    black_box(checksum);
    dts.sort_unstable();
    let n = dts.len();
    let q = |p: f64| dts[((n as f64 * p) as usize).min(n - 1)];
    let mean = dts.iter().sum::<u64>() as f64 / n as f64;
    println!(
        "[tail] {name}: n={n} mean={mean:.1}ns p50={}ns p99={}ns p999={}ns max={}ns",
        q(0.50),
        q(0.99),
        q(0.999),
        dts[n - 1]
    );
    dts[n - 1]
}

fn bench_ab_tail(c: &mut Criterion) {
    let mixed = mixed_ticks(100_000);
    let worst = worst_ticks(25_000); // 50_000 个 op
    let mut g = c.benchmark_group("l2_ab_tail");
    // 手动逐 tick 计时每个组合只跑一遍（`Once`  guard）：criterion 会反复调用
    // bench 闭包（warmup + 多 sample），重复跑 100k pass 既慢又无意义。
    // 因此本组的 criterion 计时数字是占位，真实数据看 `[tail]` 打印行。
    g.warm_up_time(Duration::from_millis(1));
    g.sample_size(10); // criterion 下限；Once 保证真实测量只跑一遍
    g.measurement_time(Duration::from_millis(10));
    static DONE: [Once; 4] = [Once::new(), Once::new(), Once::new(), Once::new()];
    macro_rules! tail_bench {
        ($name:expr, $label:expr, $ticks:expr, $make:expr, $idx:expr) => {
            g.bench_function($name, |b| {
                DONE[$idx].call_once(|| {
                    report_tail($label, $ticks, $make);
                });
                b.iter(|| black_box(0u64))
            })
        };
    }
    tail_bench!("flat_mixed", "flat/mixed", &mixed, FlatBook::new, 0);
    tail_bench!("o1_mixed", "o1/mixed", &mixed, O1Book::new, 1);
    tail_bench!("flat_worst", "flat/worst", &worst, FlatBook::new, 2);
    tail_bench!("o1_worst", "o1/worst", &worst, O1Book::new, 3);
    g.finish();
}

criterion_group!(benches, bench_ab_mean, bench_ab_tail);
criterion_main!(benches);

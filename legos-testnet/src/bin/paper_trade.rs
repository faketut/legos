//! 0 成本 paper-trading 压力测试：把泛型参数换成实盘积木。
//!
//! ```text
//! TestnetWsFeed → L2FlatArrayBook → MarketMakerStrategy → HardLimitRisk → TestnetRestGateway
//! ```
//!
//! 用法：
//!
//! ```sh
//! # 1. 免费申请 testnet key（虚拟资金，见 README「0 成本实盘验证」）
//! export BINANCE_TESTNET_API_KEY=...
//! export BINANCE_TESTNET_API_SECRET=...
//! # 2. 跑（默认 BTCUSDT，500 个 tick 后退出并打印统计）
//! cargo run -p legos-testnet --bin paper_trade -- BTCUSDT 500
//! ```
//!
//! 注意：这是 **testnet 真实下单**（虚拟资金），不是 dry-run；
//! Ctrl-C 可随时中断。

use std::env;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use legos_book::L2FlatArrayBook;
use legos_core::{AckStatus, ExecutionGateway, MarketDataFeed, OrderBook, PreTradeRisk, TradingStrategy};
use legos_risk::HardLimitRisk;
use legos_strategy::MarketMakerStrategy;
use legos_testnet::{TestnetRestGateway, TestnetWsFeed};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let symbol = args.get(1).map(|s| s.as_str()).unwrap_or("BTCUSDT");
    let max_ticks: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);

    println!("=== Legos paper trading（0 成本实盘验证）===");
    println!("symbol: {symbol}   max_ticks: {max_ticks}");
    println!("行情: Binance Testnet 免费 WS（无需 key）");

    let mut gateway = match TestnetRestGateway::new(symbol, 1) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("下单网关初始化失败：{e}");
            eprintln!();
            eprintln!("免费申请 testnet key（虚拟资金，0 成本）：");
            eprintln!("  1. 打开 https://testnet.binance.vision ，登录（可用 GitHub）");
            eprintln!("  2. 右上角 → API Management → Create API（选 HMAC 类型）");
            eprintln!("  3. export BINANCE_TESTNET_API_KEY=... BINANCE_TESTNET_API_SECRET=...");
            std::process::exit(2);
        }
    };
    println!("下单: Binance Testnet REST（虚拟资金）");
    println!("注意：testnet 真实下单（虚拟资金），非 dry-run；Ctrl-C 可随时中断。");
    println!();

    let mut feed = TestnetWsFeed::binance_testnet(symbol, 1);
    let mut book = L2FlatArrayBook::<10>::new();
    let mut strategy = MarketMakerStrategy::new(1, 50, 0, 10);
    let mut risk = HardLimitRisk::new(i64::MAX, 1_000_000, 5);

    let mut ticks = 0u64;
    let mut sent = 0u64;
    let mut rejected = 0u64;
    while ticks < max_ticks {
        let Some(ev) = feed.next_event() else {
            continue; // None = 暂时无数据
        };
        ticks += 1;
        book.apply(&ev);
        if let Some(mut intent) = strategy.on_tick(book.best_bid(), book.best_ask(), book.mid_price())
        {
            intent.ts_ns = now_ns();
            match risk.check_order(&intent) {
                Ok(()) => {
                    let ack = gateway.send_order(&intent);
                    sent += 1;
                    if ack.status == AckStatus::Rejected {
                        rejected += 1;
                    }
                    println!(
                        "order #{sent} {:?} price={} qty={} -> {:?} filled={}",
                        intent.side, intent.price, intent.qty, ack.status, ack.filled_qty
                    );
                    if let Some(e) = gateway.last_error() {
                        println!("  gateway note: {e}");
                    }
                    // testnet 有限流：每单后稍歇，避免 429。
                    thread::sleep(Duration::from_millis(300));
                }
                Err(reason) => println!("risk reject: {reason}"),
            }
        }
        if ticks % 100 == 0 {
            println!("... {ticks} ticks, {sent} orders sent");
        }
    }

    println!();
    println!(
        "done: ticks={ticks} sent={sent} rejected={rejected} \
         feed_msgs={} parse_errors={} reconnects={}",
        feed.messages, feed.parse_errors, feed.reconnects
    );
}

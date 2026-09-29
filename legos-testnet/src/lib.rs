//! `legos-testnet`：0 成本实盘验证积木。
//!
//! * [`ws::TestnetWsFeed`] — Binance Testnet 免费 WS 行情流，
//!   实现 [`MarketDataFeed`](legos_core::MarketDataFeed)（行情**无需 key**）；
//! * [`rest::TestnetRestGateway`] — Binance Testnet 免费下单 REST API，
//!   实现 [`ExecutionGateway`](legos_core::ExecutionGateway)
//!  （虚拟资金；key 只从环境变量读，绝不硬编码）。
//!
//! 两个积木输出/接受的数据契约与 [`CsvFileFeed`](https://github.com/faketut/legos)
//! 完全一致，可直接替换泛型参数做 paper-trading 压力测试。
//! 数据刻度与 side 语义见 `SPEC.md`。

pub mod rest;
pub mod ws;

pub use rest::TestnetRestGateway;
pub use ws::TestnetWsFeed;

#[cfg(test)]
mod conformance_tests {
    //! TestnetWsFeed ↔ CsvFileFeed spec 对齐测试。
    //!
    //! 同一逻辑事件（同一笔 trade）经 WS JSON 与经 CSV 解析，
    //! 得到的 MarketTick 必须逐字段一致（`order_id` 除外：CSV 无订单
    //! ID 固定为 0，WS 取 Binance trade id——与 ITCH 对齐测试相同的豁免）。

    use crate::ws::parse_trade_message;
    use legos_core::{EventKind, MarketTick, Side};

    fn ws_tick() -> MarketTick {
        let json = r#"{"e":"trade","E":1727486400001,"s":"BTCUSDT","t":424242,"p":"97500.12","q":"0.5","T":1727486400000,"m":true,"M":true}"#;
        parse_trade_message(1, json).expect("fixture 必须可解析")
    }

    fn csv_tick() -> MarketTick {
        // 与 scripts/fetch_ticks.py 对同一笔 trade 的输出逐行一致：
        // symbol_id,side,price,qty,kind,ts_ns
        let row = "1,B,9750012000000,50000000,TRADE,1727486400000000000";
        let cols: Vec<&str> = row.split(',').collect();
        assert_eq!(cols.len(), 6);
        let side = match cols[1] {
            "B" => Side::Bid,
            "A" => Side::Ask,
            _ => panic!("bad side"),
        };
        let kind = match cols[4] {
            "TRADE" => EventKind::Trade,
            _ => panic!("bad kind"),
        };
        MarketTick::new(
            cols[0].parse().unwrap(),
            cols[2].parse().unwrap(),
            cols[3].parse().unwrap(),
            side,
            kind,
            0, // CSV 无订单 ID
            cols[5].parse().unwrap(),
        )
    }

    #[test]
    fn ws_and_csv_ticks_agree_field_by_field() {
        let w = ws_tick();
        let c = csv_tick();
        assert_eq!(w.symbol_id, c.symbol_id);
        assert_eq!(w.price, c.price, "价格刻度必须一致（1 tick = 1e-8）");
        assert_eq!(w.qty, c.qty, "数量刻度必须一致（1 = 1e-8）");
        assert_eq!(w.side(), c.side(), "side 语义必须一致（m=true → Bid）");
        assert_eq!(w.kind(), c.kind());
        assert_eq!(w.ts_ns, c.ts_ns);
        assert!(w.reserved_bits_clear() && c.reserved_bits_clear());
        // order_id 豁免：WS=Binance trade id，CSV=0（见模块文档）
        assert_eq!(w.order_id, 424242);
        assert_eq!(c.order_id, 0);
    }
}

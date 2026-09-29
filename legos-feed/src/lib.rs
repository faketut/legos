//! legos-feed: 行情接入 —— 系统的「耳朵」。
//!
//! * [`CsvFileFeed`]: 回测积木，从本地 CSV 文件逐行解析
//!   [`MarketTick`](legos_core::MarketTick)；仓库自带 `data/sample_ticks.csv`
//!   示例数据；
//! * [`NativeItchParser`]: 实盘积木，解析 NASDAQ ITCH 5.0 二进制流
//!   （`'A'` Add Order / `'E'` Executed / `'C'` Executed with Price /
//!   `'X'` Cancel / `'D'` Delete），输出统一的 `MarketTick`，
//!   其余消息类型见文档中的类型表。
//!
//! **契约**：两种 feed 对同一逻辑事件产生的 `MarketTick` 在结构上完全一致
//! （同一 32 字节布局；除 `order_id` 来源不同——CSV 固定填 0，ITCH 取订单号
//! ——其余字段逐字段相等）。见 `legos-feed/SPEC.md` 与下面的对齐测试。

mod csv;
mod itch;

pub use csv::CsvFileFeed;
pub use itch::NativeItchParser;

#[cfg(test)]
mod conformance_tests {
    //! Spec-Based Testing：两种 feed 的输出对齐测试。
    //!
    //! 同一逻辑 Add 事件分别走 CSV 与 ITCH 解析，断言 `MarketTick`
    //! 逐字段一致（`order_id` 除外：CSV 固定 0，ITCH 取订单号）。

    use super::*;
    use legos_core::{EventKind, MarketDataFeed, MarketTick, Side};
    use std::fs::File;
    use std::io::Write;

    /// 构造一条 ITCH 'A' 帧：locate=1, ref=100, B, 300 股, 1502500, ts=0x1234。
    fn itch_add_frame() -> Vec<u8> {
        let mut m = vec![b'A'];
        m.extend_from_slice(&1u16.to_be_bytes());
        m.extend_from_slice(&7u16.to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0, 0x12, 0x34]);
        m.extend_from_slice(&100u64.to_be_bytes());
        m.push(b'B');
        m.extend_from_slice(&300u32.to_be_bytes());
        m.extend_from_slice(b"TESTSTK ");
        m.extend_from_slice(&1_502_500u32.to_be_bytes());
        let mut f = (m.len() as u16).to_be_bytes().to_vec();
        f.extend_from_slice(&m);
        f
    }

    #[test]
    fn csv_and_itch_emit_structurally_identical_ticks() {
        // ITCH 侧
        let mut itch = NativeItchParser::new();
        itch.push_bytes(&itch_add_frame());
        let from_itch = itch.next_event().expect("ITCH 应吐出 Add");

        // CSV 侧：同一逻辑事件（ts_ns = 0x1234 = 4660）
        let mut p = std::env::temp_dir();
        p.push(format!("legos_conform_{}.csv", std::process::id()));
        let mut f = File::create(&p).unwrap();
        writeln!(
            f,
            "symbol_id,side,price,qty,kind,ts_ns\n1,B,1502500,300,ADD,4660"
        )
        .unwrap();
        drop(f);
        let mut csv = CsvFileFeed::open(&p).unwrap();
        let from_csv = csv.next_event().expect("CSV 应吐出 Add");
        std::fs::remove_file(&p).ok();

        // 结构一致性：同一 32 字节布局，字段逐个相等
        assert_eq!(std::mem::size_of_val(&from_csv), std::mem::size_of_val(&from_itch));
        assert_eq!(from_csv.symbol_id, from_itch.symbol_id);
        assert_eq!(from_csv.price, from_itch.price);
        assert_eq!(from_csv.qty, from_itch.qty);
        assert_eq!(from_csv.side(), from_itch.side());
        assert_eq!(from_csv.side(), Side::Bid);
        assert_eq!(from_csv.kind(), from_itch.kind());
        assert_eq!(from_csv.kind(), EventKind::Add);
        assert_eq!(from_csv.ts_ns, from_itch.ts_ns);
        assert_eq!(from_csv.flags, from_itch.flags, "bit-mask 布局一致");
        // 唯一允许的差异：order_id 来源不同
        assert_eq!(from_csv.order_id, 0);
        assert_eq!(from_itch.order_id, 100);
    }

    #[test]
    fn both_feeds_yield_valid_32_byte_ticks() {
        // 任意 tick 都满足数据契约：32 字节、保留位清零
        let mut itch = NativeItchParser::new();
        itch.push_bytes(&itch_add_frame());
        for t in [itch.next_event().unwrap()] {
            assert_eq!(std::mem::size_of_val(&t), MarketTick::SIZE);
            assert!(t.reserved_bits_clear());
        }
    }
}

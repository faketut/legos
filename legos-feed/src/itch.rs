//! NASDAQ ITCH 5.0 二进制流解析（实盘积木）。
//!
//! 组播包去掉外层协议头后，ITCH 消息流的帧格式为：
//!
//! ```text
//! [u16 BE 消息长度][1 字节类型][消息体...] [u16 BE 长度][类型][消息体...] ...
//! ```
//!
//! 所有多字节整数均为大端。`symbol_id` 取消息头的 **Stock Locate**（u16，
//! 直接对应 `MarketTick.symbol_id`），价格取 ITCH 原生单位（1/10000 美元
//! 的整数），直接作为系统内的 tick 整数。
//!
//! # 已实现的消息类型
//!
//! | 类型 | 名称                 | 长度 | 转为的 `MarketTick`                  |
//! |------|----------------------|------|--------------------------------------|
//! | `'A'`| Add Order          | 36   | `Add`（方向/量/价齐全）               |
//! | `'E'`| Order Executed     | 31   | `Trade`（方向/价从 `'A'` 的记录补全） |
//! | `'C'`| Executed with Price| 36   | `Trade`（自带成交价）                 |
//! | `'X'`| Order Cancel       | 23   | `Cancel`（撤销股数）                  |
//! | `'D'`| Order Delete       | 19   | `Cancel`（整单删除）                  |
//!
//! # 未实现（跳过）的消息类型
//!
//! `'S'` 系统事件、`'R'` 证券目录、`'H'` 交易状态、`'P'` 非交叉成交、
//! `'Q'` 交叉成交、`'B'` 废单、`'U'` 订单替换、`'I'`/`'J'`/`'K'`/`'V'`/
//! `'W'`/`'Y'`/`'L'`/`'h'` 等：解析器按长度跳过，不影响后续消息，
//! 如需支持可在 `parse_message` 的 `match` 里追加分支。
//!
//! 为补全 `'E'`/`'X'`/`'D'` 的方向与价格，解析器内部维护一张
//! `order_ref -> (Side, price, 剩余股数)` 的小表（由 `'A'` 写入）。
//! 这张表只在 feed 解析阶段使用，不在纳秒热路径上。
//!
//! # 精度说明
//!
//! ITCH 的 `order_ref` 为 u64，`MarketTick.order_id` 为 u32——超出 u32 范围
//! 的订单号会被**截断**（`as u32`，低 32 位）。NASDAQ 实际 order ref 远小于
//! 2^32，此处为文档化取舍；`open` 内部表仍用完整 u64 做精确匹配。

use std::collections::HashMap;

use legos_core::{EventKind, MarketDataFeed, MarketTick, Side};

/// ITCH 5.0 流式解析器：`push_bytes` 投喂原始字节，`next_event` 逐个吐出 `MarketTick`。
pub struct NativeItchParser {
    buf: Vec<u8>,
    cursor: usize,
    /// order_ref -> (side, price, 剩余股数)
    open: HashMap<u64, (Side, i64, u32)>,
}

impl NativeItchParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            open: HashMap::new(),
        }
    }

    /// 投喂从组播 / 文件读到的原始字节（可分多次投喂，半包会被缓存）。
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        self.maybe_compact();
        self.buf.extend_from_slice(chunk);
    }

    /// 当前缓存的未解析订单数（监控用）。
    pub fn open_orders(&self) -> usize {
        self.open.len()
    }

    fn maybe_compact(&mut self) {
        // 消费指针过大时把剩余字节前移，避免 buf 无限增长。
        if self.cursor > 4096 && self.cursor * 2 > self.buf.len() {
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
    }

    /// 尝试解析一条完整消息；半包 / 无数据时返回 `None`。
    fn try_parse_one(&mut self) -> Option<MarketTick> {
        loop {
            let avail = self.buf.len() - self.cursor;
            if avail < 2 {
                return None;
            }
            let len =
                u16::from_be_bytes([self.buf[self.cursor], self.buf[self.cursor + 1]]) as usize;
            if avail < 2 + len {
                return None; // 半包：等更多字节
            }
            let msg = &self.buf[self.cursor + 2..self.cursor + 2 + len];
            self.cursor += 2 + len;
            if let Some(tick) = Self::parse_message(msg, &mut self.open) {
                return Some(tick);
            }
            // 未实现的消息类型：跳过，继续看下一条。
        }
    }

    fn parse_message(
        msg: &[u8],
        open: &mut HashMap<u64, (Side, i64, u32)>,
    ) -> Option<MarketTick> {
        let typ = *msg.first()?;
        match typ {
            b'A' => parse_add(msg, open),
            b'E' => parse_executed(msg, open, None),
            b'C' => parse_executed_with_price(msg, open),
            b'X' => parse_cancel(msg, open),
            b'D' => parse_delete(msg, open),
            _ => None, // 未实现的类型：跳过（见模块文档）
        }
    }
}

impl Default for NativeItchParser {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketDataFeed for NativeItchParser {
    fn next_event(&mut self) -> Option<MarketTick> {
        self.try_parse_one()
    }
}

// --- 字段读取小工具 ---------------------------------------------------------

fn u16_at(m: &[u8], off: usize) -> Option<u16> {
    m.get(off..off + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(m: &[u8], off: usize) -> Option<u32> {
    m.get(off..off + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(m: &[u8], off: usize) -> Option<u64> {
    m.get(off..off + 8).map(|b| {
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    })
}

/// 6 字节时间戳 -> u64（纳秒，自午夜起）。
fn ts48_at(m: &[u8], off: usize) -> Option<u64> {
    m.get(off..off + 6).map(|b| {
        ((b[0] as u64) << 40)
            | ((b[1] as u64) << 32)
            | ((b[2] as u64) << 24)
            | ((b[3] as u64) << 16)
            | ((b[4] as u64) << 8)
            | (b[5] as u64)
    })
}

fn header(m: &[u8]) -> Option<(u16, u64)> {
    // 返回 (symbol_id = stock_locate, ts_ns)
    Some((u16_at(m, 1)?, ts48_at(m, 5)?))
}

// --- 各消息类型的解析 -------------------------------------------------------

/// 'A' Add Order（36 字节）。
fn parse_add(m: &[u8], open: &mut HashMap<u64, (Side, i64, u32)>) -> Option<MarketTick> {
    if m.len() < 36 {
        return None;
    }
    let (symbol_id, ts_ns) = header(m)?;
    let order_ref = u64_at(m, 11)?;
    let side = Side::from_byte(m[19])?;
    let shares = u32_at(m, 20)?;
    let price = u32_at(m, 32)? as i64;
    open.insert(order_ref, (side, price, shares));
    Some(MarketTick::new(
        symbol_id,
        price,
        shares,
        side,
        EventKind::Add,
        order_ref as u32, // 精度说明见模块文档
        ts_ns,
    ))
}

/// 'E' Order Executed（31 字节）：方向/价格从 'A' 的记录补全。
fn parse_executed(
    m: &[u8],
    open: &mut HashMap<u64, (Side, i64, u32)>,
    price_override: Option<i64>,
) -> Option<MarketTick> {
    if m.len() < 31 {
        return None;
    }
    let (symbol_id, ts_ns) = header(m)?;
    let order_ref = u64_at(m, 11)?;
    let exec_qty = u32_at(m, 19)?;
    let (side, price, remaining) = *open.get(&order_ref)?;
    let price = price_override.unwrap_or(price);
    let left = remaining.saturating_sub(exec_qty);
    if left == 0 {
        open.remove(&order_ref);
    } else {
        open.insert(order_ref, (side, price, left));
    }
    // 成交方向：tick.side 记录的是**被吃掉的挂单**方向（账簿据此扣减对应档位）。
    Some(MarketTick::new(
        symbol_id,
        price,
        exec_qty,
        side,
        EventKind::Trade,
        order_ref as u32,
        ts_ns,
    ))
}

/// 'C' Order Executed with Price（36 字节）：自带成交价。
fn parse_executed_with_price(
    m: &[u8],
    open: &mut HashMap<u64, (Side, i64, u32)>,
) -> Option<MarketTick> {
    if m.len() < 36 {
        return None;
    }
    let price = u32_at(m, 32)? as i64;
    parse_executed(m, open, Some(price))
}

/// 'X' Order Cancel（23 字节）。
fn parse_cancel(m: &[u8], open: &mut HashMap<u64, (Side, i64, u32)>) -> Option<MarketTick> {
    if m.len() < 23 {
        return None;
    }
    let (symbol_id, ts_ns) = header(m)?;
    let order_ref = u64_at(m, 11)?;
    let cancelled = u32_at(m, 19)?;
    let (side, price, remaining) = *open.get(&order_ref)?;
    let left = remaining.saturating_sub(cancelled);
    if left == 0 {
        open.remove(&order_ref);
    } else {
        open.insert(order_ref, (side, price, left));
    }
    Some(MarketTick::new(
        symbol_id,
        price,
        cancelled,
        side,
        EventKind::Cancel,
        order_ref as u32,
        ts_ns,
    ))
}

/// 'D' Order Delete（19 字节）：整单删除。
fn parse_delete(m: &[u8], open: &mut HashMap<u64, (Side, i64, u32)>) -> Option<MarketTick> {
    if m.len() < 19 {
        return None;
    }
    let (symbol_id, ts_ns) = header(m)?;
    let order_ref = u64_at(m, 11)?;
    let (side, price, remaining) = open.remove(&order_ref)?;
    Some(MarketTick::new(
        symbol_id,
        price,
        remaining,
        side,
        EventKind::Cancel,
        order_ref as u32,
        ts_ns,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(mut msg: Vec<u8>) -> Vec<u8> {
        let mut f = (msg.len() as u16).to_be_bytes().to_vec();
        f.append(&mut msg);
        f
    }

    fn add_msg(locate: u16, order_ref: u64, side: u8, shares: u32, price: u32) -> Vec<u8> {
        let mut m = vec![b'A'];
        m.extend_from_slice(&locate.to_be_bytes());
        m.extend_from_slice(&7u16.to_be_bytes()); // tracking
        m.extend_from_slice(&[0, 0, 0, 0, 0x12, 0x34]); // timestamp
        m.extend_from_slice(&order_ref.to_be_bytes());
        m.push(side);
        m.extend_from_slice(&shares.to_be_bytes());
        m.extend_from_slice(b"TESTSTK "); // stock 8 字节
        m.extend_from_slice(&price.to_be_bytes());
        assert_eq!(m.len(), 36);
        frame(m)
    }

    #[test]
    fn parses_add_order_fields() {
        let mut p = NativeItchParser::new();
        p.push_bytes(&add_msg(42, 12345, b'B', 300, 1_502_500));
        let t = p.next_event().expect("应解析出一条 Add");
        assert_eq!(t.kind(), EventKind::Add);
        assert_eq!(t.symbol_id, 42, "symbol_id 取 stock-locate");
        assert_eq!(t.side(), Side::Bid);
        assert_eq!(t.qty, 300);
        assert_eq!(t.price, 1_502_500, "ITCH 价格单位 1/10000 美元直接作为 tick");
        assert_eq!(t.order_id, 12345);
        assert_eq!(t.ts_ns, 0x1234);
        assert_eq!(p.open_orders(), 1);
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn parses_executed_cancel_delete_lifecycle() {
        let mut p = NativeItchParser::new();
        p.push_bytes(&add_msg(1, 100, b'S', 500, 2_000_000));

        // 'E' executed 200 股
        let mut e = vec![b'E'];
        e.extend_from_slice(&1u16.to_be_bytes());
        e.extend_from_slice(&0u16.to_be_bytes());
        e.extend_from_slice(&[0, 0, 0, 0, 0, 9]);
        e.extend_from_slice(&100u64.to_be_bytes());
        e.extend_from_slice(&200u32.to_be_bytes());
        e.extend_from_slice(&999u64.to_be_bytes()); // match number
        assert_eq!(e.len(), 31);
        p.push_bytes(&frame(e));

        // 'X' cancel 100 股
        let mut x = vec![b'X'];
        x.extend_from_slice(&1u16.to_be_bytes());
        x.extend_from_slice(&0u16.to_be_bytes());
        x.extend_from_slice(&[0, 0, 0, 0, 0, 10]);
        x.extend_from_slice(&100u64.to_be_bytes());
        x.extend_from_slice(&100u32.to_be_bytes());
        assert_eq!(x.len(), 23);
        p.push_bytes(&frame(x));

        // 'D' 删除剩余
        let mut d = vec![b'D'];
        d.extend_from_slice(&1u16.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&[0, 0, 0, 0, 0, 11]);
        d.extend_from_slice(&100u64.to_be_bytes());
        assert_eq!(d.len(), 19);
        p.push_bytes(&frame(d));

        let a = p.next_event().unwrap();
        assert_eq!(a.kind(), EventKind::Add);
        let t = p.next_event().unwrap();
        assert_eq!(t.kind(), EventKind::Trade);
        assert_eq!(t.qty, 200);
        assert_eq!(t.side(), Side::Ask, "被吃掉的是卖单");
        assert_eq!(t.price, 2_000_000, "价格从 Add 记录补全");
        let c = p.next_event().unwrap();
        assert_eq!((c.kind(), c.qty), (EventKind::Cancel, 100));
        let del = p.next_event().unwrap();
        assert_eq!((del.kind(), del.qty), (EventKind::Cancel, 200), "剩余 500-200-100=200");
        assert_eq!(p.open_orders(), 0);
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn half_packet_waits_for_rest() {
        let mut p = NativeItchParser::new();
        let full = add_msg(1, 7, b'B', 10, 100_0000);
        p.push_bytes(&full[..5]); // 半包
        assert_eq!(p.next_event(), None, "半包不能吐出事件");
        p.push_bytes(&full[5..]); // 补齐
        let t = p.next_event().expect("补全后应解析出 Add");
        assert_eq!(t.order_id, 7);
        assert_eq!(t.price, 100_0000);
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn unknown_message_type_is_skipped() {
        // 'S' 系统事件（12 字节）：解析器按长度跳过，不影响后续消息。
        let mut s = vec![b'S'];
        s.extend_from_slice(&0u16.to_be_bytes());
        s.extend_from_slice(&0u16.to_be_bytes());
        s.extend_from_slice(&[0, 0, 0, 0, 0, 1]);
        s.push(b'O');
        assert_eq!(s.len(), 12);

        let mut stream = frame(s);
        stream.extend_from_slice(&add_msg(1, 8, b'B', 10, 100_0000));
        let mut p = NativeItchParser::new();
        p.push_bytes(&stream);
        let t = p.next_event().expect("未知类型应被跳过，后续 Add 正常解析");
        assert_eq!(t.order_id, 8);
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn sell_side_indicator_maps_to_ask() {
        let mut p = NativeItchParser::new();
        p.push_bytes(&add_msg(3, 1, b'S', 1, 5));
        assert_eq!(p.next_event().unwrap().side(), Side::Ask);
    }
}

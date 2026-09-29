//! FIX 4.4 执行网关（实盘积木）。
//!
//! 把 [`OrderIntent`](legos_core::OrderIntent) 编码为 FIX 4.4 `NewOrderSingle`
//!（`35=D`），经 `TcpStream` 发送。设计要点：
//!
//! * 连接是**惰性**的：`new` 只保存配置，第一次 `send_order` 时才建连；
//! * 任何网络失败（建连超时 / 写入失败）都**不 panic**，返回
//!   `AckStatus::Rejected` 的 [`OrderAck`](legos_core::OrderAck)；
//! * `BodyLength(9)` / `CheckSum(10)` 按 FIX 规范计算，字段间用 SOH(`\x01`) 分隔。

use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use legos_core::{AckStatus, ExecutionGateway, OrderAck, OrderIntent, Side};

const SOH: char = '\x01';

/// FIX 4.4 新订单网关。
pub struct FixProtocolGateway {
    endpoint: String,
    sender_comp_id: String,
    target_comp_id: String,
    seq_num: u64,
    stream: Option<TcpStream>,
    connect_timeout: Duration,
}

impl FixProtocolGateway {
    /// `endpoint` 如 `"127.0.0.1:9878"`；`sender`/`target` 为 `49`/`56` 域。
    pub fn new(endpoint: &str, sender_comp_id: &str, target_comp_id: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            sender_comp_id: sender_comp_id.to_string(),
            target_comp_id: target_comp_id.to_string(),
            seq_num: 0,
            stream: None,
            connect_timeout: Duration::from_secs(2),
        }
    }

    /// 当前发送序号（`34` 域）。
    pub fn seq_num(&self) -> u64 {
        self.seq_num
    }

    /// 构造一条完整的 FIX 4.4 NewOrderSingle 线上报文（含 8/9/10 头尾）。
    ///
    /// 市价单（`price == 0`）用 `40=1`，限价单用 `40=2`。
    /// 公开为 `pub` 以便单测 / 审计报文内容。
    pub fn build_new_order_single(&mut self, order: &OrderIntent) -> String {
        self.seq_num += 1;
        let side = match order.side {
            Side::Bid => "1",
            Side::Ask => "2",
        };
        let (ord_type, price_field) = if order.price == 0 {
            ("1".to_string(), String::new()) // 市价：40=1，无 44 域
        } else {
            ("2".to_string(), format!("44={}{}", order.price, SOH))
        };
        let symbol = format!("SYM{}", order.symbol_id);
        let body = format!(
            "35=D{S}49={sender}{S}56={target}{S}34={seq}{S}52={ts}{S}\
             11={clord}{S}21=1{S}40={ord_type}{S}54={side}{S}55={symbol}{S}\
             60={ts}{S}38={qty}{S}{price_field}",
            S = SOH,
            sender = self.sender_comp_id,
            target = self.target_comp_id,
            seq = self.seq_num,
            ts = utc_timestamp(),
            clord = order.client_order_id,
            ord_type = ord_type,
            side = side,
            symbol = symbol,
            qty = order.qty,
            price_field = price_field,
        );
        let header = format!("8=FIX.4.4{S}9={len:05}{S}", S = SOH, len = body.len());
        let checksum = (header.bytes().chain(body.bytes()).map(|b| b as u32).sum::<u32>() % 256) as u8;
        format!("{header}{body}10={ck:03}{S}", ck = checksum, S = SOH)
    }

    /// 确保已连接；失败返回 `Err`（调用方转为 Rejected 回执）。
    fn ensure_connected(&mut self) -> std::io::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let addr = self
            .endpoint
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "无法解析 endpoint"))?;
        let stream = TcpStream::connect_timeout(&addr, self.connect_timeout)?;
        stream.set_nodelay(true)?;
        self.stream = Some(stream);
        Ok(())
    }
}

impl ExecutionGateway for FixProtocolGateway {
    fn send_order(&mut self, order: &OrderIntent) -> OrderAck {
        // 永不 panic：任何失败都降级为 Rejected 回执。
        if self.ensure_connected().is_err() {
            self.stream = None;
            return OrderAck::rejected(order.client_order_id);
        }
        let msg = self.build_new_order_single(order);
        let ok = self
            .stream
            .as_mut()
            .map(|s| s.write_all(msg.as_bytes()).is_ok())
            .unwrap_or(false);
        if !ok {
            self.stream = None; // 断线：下次重连
            return OrderAck::rejected(order.client_order_id);
        }
        OrderAck {
            client_order_id: order.client_order_id,
            status: AckStatus::Accepted,
            filled_qty: 0,
            avg_price: 0,
        }
    }
}

/// `52`/`60` 域用的 UTC 时间戳：`YYYYMMDD-HH:MM:SS.sss`。
fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let ms = now.subsec_millis();
    // 用 chrono 太重：手动从 unix 时间戳换算年月日（1970-01-01 起）。
    let days = secs / 86400;
    let sod = secs % 86400;
    let (y, m, d) = ymd_from_days(days);
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}.{:03}",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        ms
    )
}

/// 天数（自 1970-01-01）-> (年, 月, 日)。Howard Hinnant 算法。
fn ymd_from_days(z: u64) -> (u64, u64, u64) {
    let z = z as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u64;
    ((if m <= 2 { y + 1 } else { y }) as u64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    fn intent() -> OrderIntent {
        OrderIntent {
            client_order_id: 777,
            symbol_id: 1,
            side: Side::Bid,
            price: 100_5000,
            qty: 100,
            ts_ns: 0,
        }
    }

    /// 校验报文的 BodyLength(9) 与 CheckSum(10) 自洽。
    fn verify_fix_envelope(msg: &str, expect_ord_type: &str) {
        assert!(msg.starts_with("8=FIX.4.4\x01"), "报文头");
        // 9= 后 5 位是 body 长度
        let body_len: usize = msg[12..17].parse().unwrap();
        // body = 9= 域之后、10= 域之前
        let body_start = msg.find("\x0135=").unwrap() + 1;
        let cksum_pos = msg.rfind("\x0110=").unwrap() + 1;
        assert_eq!(body_len, msg[body_start..cksum_pos].len(), "9= 自洽");
        let expect: u32 = msg[..cksum_pos].bytes().map(|b| b as u32).sum::<u32>() % 256;
        // cksum_pos 已指向 "10=" 的 '1'（rfind 返回 \x01 位置 +1），数字在 +3..+6。
        let got: u32 = msg[cksum_pos + 3..cksum_pos + 6].parse().unwrap();
        assert_eq!(expect, got, "10= 自洽");
        assert!(msg.contains("\x0135=D\x01"), "35=D");
        assert!(msg.contains("\x0154=1\x01"), "54=1 (buy)");
        assert!(
            msg.contains(&format!("\x0140={expect_ord_type}\x01")),
            "40= ord_type"
        );
    }

    #[test]
    fn new_order_single_envelope_is_consistent() {
        let mut gw = FixProtocolGateway::new("127.0.0.1:1", "SENDER", "TARGET");
        let msg = gw.build_new_order_single(&intent());
        verify_fix_envelope(&msg, "2");
        assert!(msg.contains("\x0111=777\x01"), "11=ClOrdID");
        assert_eq!(gw.seq_num(), 1);
        let msg2 = gw.build_new_order_single(&intent());
        assert!(msg2.contains("\x0134=2\x01"), "序号递增");
    }

    #[test]
    fn market_order_uses_ordtype_1_without_price() {
        let mut gw = FixProtocolGateway::new("127.0.0.1:1", "S", "T");
        let mut o = intent();
        o.price = 0;
        let msg = gw.build_new_order_single(&o);
        assert!(msg.contains("\x0140=1\x01"), "市价 40=1");
        assert!(!msg.contains("\x0144="), "市价无 44 域");
        verify_fix_envelope(&msg, "1");
    }

    #[test]
    fn connection_failure_returns_rejected_not_panic() {
        // 端口 1 几乎必然拒绝连接：必须返回 Rejected 而不是 panic。
        let mut gw = FixProtocolGateway::new("127.0.0.1:1", "S", "T");
        let ack = gw.send_order(&intent());
        assert_eq!(ack.status, AckStatus::Rejected);
        assert_eq!(ack.client_order_id, 777);
    }

    #[test]
    fn send_over_loopback_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let mut gw = FixProtocolGateway::new(&addr.to_string(), "ME", "EXCH");
        let ack = gw.send_order(&intent());
        assert_eq!(ack.status, AckStatus::Accepted);
        let received = handle.join().unwrap();
        verify_fix_envelope(&received, "2");
    }
}

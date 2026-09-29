//! CSV 行情文件接入（回测积木）。
//!
//! 文件格式（首行为表头）：
//!
//! ```text
//! symbol_id,side,price,qty,kind,ts_ns
//! 1,B,1000000,100,ADD,1710000000000000000
//! ```
//!
//! * `side`: `B` = 买，`A` = 卖；
//! * `kind`: `ADD` / `CANCEL` / `TRADE`；
//! * `price`: 整数 tick；`ts_ns`: 纳秒时间戳。
//!
//! 解析失败的行会被**跳过**（行号可通过 [`CsvFileFeed::lines_read`] /
//! [`CsvFileFeed::lines_skipped`] 观察），`next_event` 在 EOF 时永久返回 `None`。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use legos_core::{EventKind, MarketDataFeed, Side, Tick};

/// 逐行解析 CSV 的行情 feed。
pub struct CsvFileFeed {
    reader: BufReader<File>,
    lines_read: u64,
    lines_skipped: u64,
    buf: String,
}

impl CsvFileFeed {
    /// 打开文件并跳过表头行。
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut header = String::new();
        reader.read_line(&mut header)?; // 表头
        Ok(Self {
            reader,
            lines_read: 0,
            lines_skipped: 0,
            buf: String::new(),
        })
    }

    /// 已成功解析的行数。
    pub fn lines_read(&self) -> u64 {
        self.lines_read
    }

    /// 因格式错误跳过的行数。
    pub fn lines_skipped(&self) -> u64 {
        self.lines_skipped
    }

    fn parse_line(line: &str) -> Option<Tick> {
        let mut f = line.split(',');
        let symbol_id: u32 = f.next()?.trim().parse().ok()?;
        let side = match f.next()?.trim() {
            "B" => Side::Bid,
            "A" => Side::Ask,
            _ => return None,
        };
        let price: i64 = f.next()?.trim().parse().ok()?;
        let qty: u64 = f.next()?.trim().parse().ok()?;
        let kind = match f.next()?.trim() {
            "ADD" => EventKind::Add,
            "CANCEL" => EventKind::Cancel,
            "TRADE" => EventKind::Trade,
            _ => return None,
        };
        let ts_ns: u64 = f.next()?.trim().parse().ok()?;
        Some(Tick {
            symbol_id,
            price,
            qty,
            side,
            kind,
            order_id: 0, // CSV 格式不携带订单号；需要时可用行号回填
            ts_ns,
        })
    }
}

impl MarketDataFeed for CsvFileFeed {
    fn next_event(&mut self) -> Option<Tick> {
        loop {
            self.buf.clear();
            let n = self.reader.read_line(&mut self.buf).ok()?;
            if n == 0 {
                return None; // EOF
            }
            let line = self.buf.trim();
            if line.is_empty() {
                continue;
            }
            match Self::parse_line(line) {
                Some(t) => {
                    self.lines_read += 1;
                    return Some(t);
                }
                None => {
                    self.lines_skipped += 1;
                    continue; // 坏行跳过，继续下一行
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_csv(tag: &str, content: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // 文件名带测试标签：同一进程内并行测试互不干扰。
        p.push(format!("legos_csv_test_{}_{}.csv", tag, std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn parses_sample_file_end_to_end() {
        let path = write_temp_csv(
            "basic",
            "symbol_id,side,price,qty,kind,ts_ns\n\
             1,B,1000000,100,ADD,10\n\
             2,A,1000500,50,TRADE,20\n",
        );
        let mut feed = CsvFileFeed::open(&path).unwrap();
        let t1 = feed.next_event().unwrap();
        assert_eq!(t1.symbol_id, 1);
        assert_eq!(t1.side, Side::Bid);
        assert_eq!(t1.kind, EventKind::Add);
        assert_eq!((t1.price, t1.qty, t1.ts_ns), (1000000, 100, 10));
        let t2 = feed.next_event().unwrap();
        assert_eq!(t2.kind, EventKind::Trade);
        assert_eq!(feed.next_event(), None);
        assert_eq!(feed.next_event(), None, "EOF 后保持 None");
        assert_eq!(feed.lines_read(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn skips_malformed_lines() {
        let path = write_temp_csv(
            "malformed",
            "symbol_id,side,price,qty,kind,ts_ns\n\
             1,B,1000000,100,ADD,10\n\
             this,is,a,bad,line\n\
             1,X,1000000,100,ADD,10\n\
             1,B,999000,5,CANCEL,30\n",
        );
        let mut feed = CsvFileFeed::open(&path).unwrap();
        assert!(feed.next_event().is_some());
        let t = feed.next_event().unwrap();
        assert_eq!(t.kind, EventKind::Cancel);
        assert_eq!(feed.next_event(), None);
        assert_eq!(feed.lines_skipped(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_is_io_error() {
        assert!(CsvFileFeed::open("/no/such/file.csv").is_err());
    }
}

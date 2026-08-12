//! JSONL 会话适配器共用的纯函数。
//!
//! claude / codex / omp 三家各自的会话文件都带 ISO-8601 时间戳,解析规则
//! 逐字节相同——按 crate 内「重复只该跨 crate 边界发生一次」的约定收在这一份,
//! 别让第四份拷贝落进新适配器。`extract_text` 的段词汇表三家各不相同,
//! 留在各适配器自己手里。

/// 解析 `2026-08-07T07:04:38.708Z` 形式的 UTC 时间戳为 Unix 毫秒。
/// 只认 Z 结尾的 ISO-8601;解析不动就 None,不报错。
pub(crate) fn iso8601_to_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    if d.next().is_some() {
        return None;
    }
    let (hms, frac) = match time.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time, ""),
    };
    let mut t = hms.split(':');
    let (h, mi, sec) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() {
        return None;
    }
    // 小数秒截到毫秒,不足三位右补零。
    let mut ms = 0i64;
    for i in 0..3 {
        let digit = frac.as_bytes().get(i).copied();
        match digit {
            Some(b @ b'0'..=b'9') => ms = ms * 10 + i64::from(b - b'0'),
            Some(_) => return None,
            None => ms *= 10,
        }
    }
    let days = days_from_civil(y, mo, day);
    Some((((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}

/// 公历日期 -> 距 1970-01-01 的天数(Howard Hinnant 的 days_from_civil)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::iso8601_to_ms;

    #[test]
    fn omp_jsonl_common_ts_parse() {
        assert_eq!(iso8601_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            iso8601_to_ms("2026-08-01T10:00:00Z"),
            Some(1_785_578_400_000)
        );
        assert_eq!(
            iso8601_to_ms("2026-08-01T10:00:00.5Z"),
            Some(1_785_578_400_500)
        );
        assert_eq!(iso8601_to_ms("垃圾"), None);
    }
}

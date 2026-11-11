//! Unix 秒 → RFC3339 UTC "Z" 字符串。**必须产 "Z"**(末尾大写 Z,无 +00:00 偏移),
//! 否则 gw-app `parse_rfc3339_unix`(只剥 Z)解析失败 → has_fresh_token 静默禁刷新。
//! 移植 gw-kiro/src/token.rs:237 format_unix_utc 的等价算法,避免跨 crate 依赖。

pub fn format_rfc3339_z(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // civil_from_days(epoch 1970-01-01)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0,146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0,365]
    let mp = (5 * doy + 2) / 153; // [0,11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1,31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1,12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// RFC3339 UTC 字符串 → Unix 秒(`format_rfc3339_z` 的逆;容忍小数秒与 `+00:00`)。
/// 只接受 UTC:带非零时区偏移的串返回 None,不猜。
pub fn parse_rfc3339_z(s: &str) -> Option<i64> {
    let s = s.trim();
    let body = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = body.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // days_from_civil(epoch 1970-01-01)
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_is_inverse_of_format() {
        for t in [0_i64, 1_780_531_200, 1_791_224_192, 951_782_400 /* 2000-02-29 */] {
            assert_eq!(parse_rfc3339_z(&format_rfc3339_z(t)), Some(t), "{t}");
        }
        assert_eq!(parse_rfc3339_z("2026-06-04T00:00:00.123Z"), Some(1_780_531_200));
        assert_eq!(parse_rfc3339_z("2026-06-04T00:00:00+00:00"), Some(1_780_531_200));
        assert_eq!(parse_rfc3339_z("2026-06-04T00:00:00+08:00"), None, "非 UTC 不猜");
        assert_eq!(parse_rfc3339_z("garbage"), None);
        assert_eq!(parse_rfc3339_z("2026-13-01T00:00:00Z"), None);
    }
    #[test]
    fn formats_unix_to_rfc3339_z() {
        // 与 gw-kiro format_unix_utc 同向量
        assert_eq!(format_rfc3339_z(1_780_531_200), "2026-06-04T00:00:00Z");
        assert_eq!(format_rfc3339_z(0), "1970-01-01T00:00:00Z");
    }
}

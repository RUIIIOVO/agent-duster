//! zstd 单文件压缩/解压，以及 UTC 时间戳。
//!
//! 只做「一个文件 <-> 一个 `.zst`」这一件事。多文件打包走 [`crate::archive`]
//! （tar + zstd）。两者刻意分开：
//! - 归档是「删除前留个证据包」，一次多个路径，用户 `tar -xf` 自行还原；
//! - 单文件压缩是「原地瘦身」（会话存档），压完 duster 自己还要能读回来，
//!   所以必须是可寻址的单文件格式，不能套一层 tar。

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// 会话存档用的压缩等级。19 是 zstd 的高压缩档（非 ultra），
/// 本机实测 36.6 MB 的 skill 文本压到 7.9 MB；会话 jsonl 冗余更高，比值更好。
pub const ARCHIVE_LEVEL: i32 = 19;

/// 流式压缩 `src` 到 `dst`，返回写出的字节数。
///
/// 要求：
/// - 流式读写，不把整个文件读进内存（会话文件可达数百 MB）；
/// - `dst` 走原子写语义（同目录 temp + rename），中途失败不留半个 `.zst`；
/// - `dst` 已存在则报错，绝不静默覆盖。
pub fn compress_file(src: &Path, dst: &Path, level: i32) -> Result<u64> {
    // 先拒绝撞名再动手：压缩的语义是「给这份内容换个存放形式」，目标已存在
    // 说明上一次操作没收干净，静默覆盖会把别人的数据抹掉且无从追回。
    // 用 symlink_metadata 而非 exists()——断链也算占位。
    if dst.symlink_metadata().is_ok() {
        bail!("refusing to overwrite existing {}", dst.display());
    }

    let file = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;
    let mut reader = BufReader::new(file);

    write_stream_atomic(dst, |w| {
        // copy_encode 内部按固定缓冲区搬运，源文件多大都不会整读进内存。
        zstd::stream::copy_encode(&mut reader, w, level)
            .with_context(|| format!("failed to compress {}", src.display()))
    })
}

/// 同目录 temp + rename 的**流式**原子写，返回落盘字节数。
///
/// 与 [`crate::atomic::write_atomic`] 是同一套纪律（`.tmp-duster-` 前缀、
/// `sync_all`、fsync 父目录），区别只在内容由回调流式产生而不是先在内存里
/// 凑成一个 `&[u8]`——压缩/解压/打包的数据量可达数百 MB。
///
/// 回调返回 `Err` 时 temp 文件随 `Drop` 删除，目录里不留 `.tmp-*` 残渣，
/// 目标路径也不会出现半个文件。[`crate::archive`] 复用同一实现。
pub(crate) fn write_stream_atomic(
    dst: &Path,
    fill: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<u64> {
    let dir = dst
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-duster-")
        .tempfile_in(&dir)
        .with_context(|| format!("failed to create temp file in {}", dir.display()))?;

    {
        let mut w = BufWriter::new(tmp.as_file_mut());
        fill(&mut w)?;
        w.flush().context("failed to flush temp file")?;
    }
    tmp.as_file()
        .sync_all()
        .context("failed to fsync temp file")?;

    let bytes = tmp
        .as_file()
        .metadata()
        .context("failed to stat temp file")?
        .len();

    tmp.persist(dst)
        .with_context(|| format!("failed to rename to {}", dst.display()))?;

    // fsync 父目录，让 rename 产生的目录项变更真正落盘。
    let dir_fd = File::open(&dir)
        .with_context(|| format!("failed to open parent directory {}", dir.display()))?;
    dir_fd
        .sync_all()
        .with_context(|| format!("failed to fsync parent directory {}", dir.display()))?;

    Ok(bytes)
}

/// 解压 `src`（`.zst`）到内存。
///
/// 给 `duster open` / `session show` 回读压缩后的会话用：byte_off/byte_len
/// 一律指向**解压后**的逻辑内容，压缩对上层完全透明。
pub fn decompress_to_vec(src: &Path) -> Result<Vec<u8>> {
    let file = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;
    let mut out = Vec::new();
    zstd::stream::copy_decode(BufReader::new(file), &mut out)
        .with_context(|| format!("failed to decompress {}", src.display()))?;
    Ok(out)
}

/// 解压 `src` 到 `dst` 文件（流式）。往返校验与手工还原都用它。
///
/// 与 [`compress_file`] 不同，这里**允许覆盖已存在的 `dst`**：还原/校验的目标
/// 是「让这个路径等于解压结果」，rename 语义天然满足，且中途失败不会破坏原文件。
pub fn decompress_to_file(src: &Path, dst: &Path) -> Result<u64> {
    let file = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;
    let mut reader = BufReader::new(file);
    write_stream_atomic(dst, |w| {
        zstd::stream::copy_decode(&mut reader, w)
            .with_context(|| format!("failed to decompress {}", src.display()))
    })
}

/// 路径是否以 `.zst` 结尾。
pub fn is_zst(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

/// UTC 时间戳 `YYYYMMDD-HHMMSS`，用于归档包名与快照目录名。
///
/// 刻意用 UTC 而不是本地时间：不引时区依赖，排序即时序，跨机器可比。
pub fn stamp(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Unix 天数 -> (年, 月, 日)。Howard Hinnant 的 civil_from_days，纯整数运算。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_matches_known_epoch() {
        // 2021-01-01T00:00:00Z = 1609459200
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_609_459_200);
        assert_eq!(stamp(t), "20210101-000000");
        // 1970-01-01T00:00:01Z
        assert_eq!(
            stamp(UNIX_EPOCH + std::time::Duration::from_secs(1)),
            "19700101-000001"
        );
    }

    /// 确定性伪随机字节：不引 rand 依赖，测试每次跑出的内容完全一致。
    /// 用 LCG 而不是重复 pattern——重复 pattern 会被 zstd 压成几百字节，
    /// 测不出流式搬运在大体量下的行为。
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        while out.len() < len {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            out.extend_from_slice(&(s >> 24).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn tmp_residue(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect()
    }

    #[test]
    fn 多兆字节文件压缩解压往返一致() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("session.jsonl");
        let zst = dir.path().join("session.jsonl.zst");
        let back = dir.path().join("roundtrip.jsonl");

        let data = pseudo_random(3 * 1024 * 1024);
        std::fs::write(&src, &data).unwrap();
        let before = crate::hash::hash_file(&src).unwrap();

        // 往返一致性与压缩等级无关，这里用低等级把 3 MB 不可压数据的测试
        // 时间压下来；ARCHIVE_LEVEL 的可用性由下面的小样本用例覆盖。
        let written = compress_file(&src, &zst, 3).unwrap();
        assert_eq!(written, std::fs::metadata(&zst).unwrap().len());

        assert_eq!(decompress_to_vec(&zst).unwrap(), data);

        let out = decompress_to_file(&zst, &back).unwrap();
        assert_eq!(out, data.len() as u64);
        assert_eq!(crate::hash::hash_file(&back).unwrap(), before);
        assert!(tmp_residue(dir.path()).is_empty());
    }

    #[test]
    fn 归档等级可用且能读回() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let zst = dir.path().join("a.txt.zst");
        let body = "{\"role\":\"user\"}\n".repeat(4096);
        std::fs::write(&src, &body).unwrap();

        let written = compress_file(&src, &zst, ARCHIVE_LEVEL).unwrap();
        // 高冗余文本应当显著变小，否则说明等级没生效。
        assert!(
            written < body.len() as u64 / 10,
            "compressed to {written} bytes"
        );
        assert_eq!(decompress_to_vec(&zst).unwrap(), body.as_bytes());
        assert!(is_zst(&zst));
        assert!(!is_zst(&src));
    }

    #[test]
    fn 拒绝覆盖已存在目标且不留临时文件() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let zst = dir.path().join("a.txt.zst");
        std::fs::write(&src, b"hello").unwrap();
        std::fs::write(&zst, b"PRECIOUS").unwrap();

        let err = compress_file(&src, &zst, 3).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"), "{err}");
        // 原有内容一个字节都不能动。
        assert_eq!(std::fs::read(&zst).unwrap(), b"PRECIOUS");
        assert!(tmp_residue(dir.path()).is_empty());
    }

    #[test]
    fn 源文件缺失时报错且不产出目标() {
        let dir = tempfile::tempdir().unwrap();
        let zst = dir.path().join("missing.zst");
        assert!(compress_file(&dir.path().join("nope"), &zst, 3).is_err());
        assert!(!zst.exists());
        assert!(tmp_residue(dir.path()).is_empty());
    }
}

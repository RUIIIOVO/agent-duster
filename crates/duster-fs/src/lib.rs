//! 文件系统基座层。
//!
//! 职责:并行目录遍历、BLAKE3 指纹、原子写入、SQLite/进程锁探测、
//! 删除前归档(tar+zstd)与配置文件快照。
//! 本 crate 不理解任何 agent 语义——语义属于适配层。

pub mod archive;
pub mod atomic;
pub mod hash;
pub mod lockprobe;
pub mod path;
pub mod snapshot;
pub mod walk;
pub mod zst;

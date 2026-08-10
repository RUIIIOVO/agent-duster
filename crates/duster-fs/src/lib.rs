//! 文件系统基座层。
//!
//! 职责：并行目录遍历、BLAKE3 指纹、原子写入、SQLite/进程锁探测、回收站。
//! 本 crate 不理解任何 agent 语义——语义属于适配层。

pub mod trash;
pub mod walk;

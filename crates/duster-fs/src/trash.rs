//! 回收站：`~/.agent-duster/trash/<timestamp>/` + manifest.json，支持原路还原。
//! 所有破坏性删除必须经过这里，禁止直接 unlink。

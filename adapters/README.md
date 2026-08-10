# adapters/

内置声明式适配器清单（`<agent-id>.toml`），编译期嵌入二进制。

用户自定义清单放 `~/.agent-duster/adapters/`，同 id 可覆盖内置。
新增一个 agent 通常只需在这里加一个 TOML 文件——不写代码、不重新编译。

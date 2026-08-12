#!/bin/zsh
# Agent 环境快照:利用 APFS clonefile(cp -c)做秒级、近零磁盘成本的备份/恢复。
# 用途:duster 破坏性命令(clean/prune/uninstall)测试前打快照,出问题一键还原。
#
#   ./scripts/env-snapshot.sh backup            # 打一份新快照
#   ./scripts/env-snapshot.sh list              # 列出所有快照
#   ./scripts/env-snapshot.sh restore <名称> [目标]   # 还原(默认还原全部目标)
#
# 快照存放:~/agent-env-snapshots/<时间戳>/
set -euo pipefail

# 被快照的目标:Tier1 四家 + 未认领目录 + Claude 全局 MCP 配置文件。
TARGETS=(
  .claude .claude.json .codex .gemini .omp
  .cursor .copilot .kimi .opencode .qoder .cc-switch .pi .gstack .baoyu-skills
)
ROOT="$HOME/agent-env-snapshots"

case "${1:-}" in
  backup)
    snap="$ROOT/$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$snap"
    for t in "${TARGETS[@]}"; do
      src="$HOME/$t"
      [[ -e "$src" || -L "$src" ]] || continue
      # -c = clonefile(APFS 写时复制):瞬间完成,共享数据块,不占双倍磁盘。
      # -R 递归 -p 保元数据;符号链接按链接本体复制,不跟随。
      cp -Rpc "$src" "$snap/$t"
      echo "  已快照 ~/$t"
    done
    echo "完成: $snap"
    du -sh "$snap" | awk '{print "快照逻辑体积: " $1 "(APFS 克隆,实际新增磁盘占用≈0)"}'
    ;;
  list)
    ls -1 "$ROOT" 2>/dev/null || echo "(还没有快照)"
    ;;
  restore)
    name="${2:?用法: restore <快照名称> [单个目标,如 .claude]}"
    snap="$ROOT/$name"
    [[ -d "$snap" ]] || { echo "快照不存在: $snap" >&2; exit 1; }
    if [[ -n "${3:-}" ]]; then targets=("$3"); else targets=("${TARGETS[@]}"); fi
    for t in "${targets[@]}"; do
      src="$snap/$t"
      [[ -e "$src" || -L "$src" ]] || continue
      dst="$HOME/$t"
      # 先把当前状态挪到旁边(防止还原本身造成二次事故),再克隆回来。
      if [[ -e "$dst" || -L "$dst" ]]; then
        mv "$dst" "$dst.pre-restore.$$"
      fi
      cp -Rpc "$src" "$dst"
      rm -rf "$dst.pre-restore.$$"
      echo "  已还原 ~/$t"
    done
    echo "还原完成(来自 $name)"
    ;;
  *)
    echo "用法: $0 backup | list | restore <名称> [目标]" >&2
    exit 1
    ;;
esac

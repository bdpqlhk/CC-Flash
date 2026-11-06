// 独立快照只判断是否出现新内容；提醒是否已读由主窗的实际前台状态决定。
export function createPetActivityTracker() {
  let files = null;
  let lastMessage = null;

  return {
    observeFiles(items) {
      const next = new Map(items.map(f => [f.name, `${f.size}:${f.modified}`]));
      const changed = files !== null && [...next].some(([name, stamp]) => files.get(name) !== stamp);
      files = next;
      return changed;
    },
    observeMessages(items) {
      const latest = items[items.length - 1];
      // 服务端最多保留 200 条，满额后条数不再增加；ID + 时间戳仍会变化。
      const next = latest ? `${latest.id}:${latest.timestamp}` : "";
      const changed = lastMessage !== null && next !== "" && next !== lastMessage;
      lastMessage = next;
      return changed;
    },
  };
}

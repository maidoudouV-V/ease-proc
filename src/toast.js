export function createToastStore() {
  return {
    items: [],

    // 添加弹窗
    add(type, title, message, duration = 5000) {
      const id = Date.now() + Math.random();
      const toast = {
        id,
        type,
        title,
        message,
        status: "entering",
      };

      this.items.push(toast);

      // 自动关闭计时器
      if (duration > 0) {
        toast._timer = setTimeout(() => {
          this.close(id);
        }, duration);
      }
    },

    // 触发关闭（播放离场动画）
    close(id) {
      const toast = this.items.find((t) => t.id === id);
      if (toast && toast.status !== "closing") {
        if (toast._timer) clearTimeout(toast._timer);
        toast.status = "closing";
      }
    },

    // 动画结束回调（真正移除数据）
    onAnimationEnd(id) {
      const idx = this.items.findIndex((t) => t.id === id);
      if (idx === -1) return;

      const toast = this.items[idx];
      if (toast.status === "entering") {
        toast.status = "idle"; // 进场结束 -> 静止
      } else if (toast.status === "closing") {
        this.items.splice(idx, 1); // 离场结束 -> 删除
      }
    },

    // 快捷方法
    success(title, msg) {
      this.add("success", title, msg);
    },
    error(title, msg) {
      this.add("error", title, msg);
    },
    info(title, msg) {
      this.add("info", title, msg);
    },
    warning(title, msg) {
      this.add("warning", title, msg);
    },
  };
}

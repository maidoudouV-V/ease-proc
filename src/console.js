const { appWindow } = window.__TAURI__.window;
const { writeText } = window.__TAURI__.clipboard;
const { invoke } = window.__TAURI__.tauri;
const { listen } = window.__TAURI__.event;
// 状态变量
let consoleLines = [];
let lineCount = 0;
let autoScrollEnabled = true;
let showTimestamp = false;
let isWaitingForStream = true;

// DOM 元素引用
const consoleOutput = document.getElementById("console-output");
// const lineCountEl = document.getElementById("line-count");
const autoScrollCheckbox = document.getElementById("auto-scroll");
const showTimestampCheckbox = document.getElementById("show-timestamp");

// 窗口控制逻辑
document
  .getElementById("win-minimize")
  ?.addEventListener("click", () => appWindow.minimize());
document
  .getElementById("win-maximize")
  ?.addEventListener("click", () => appWindow.toggleMaximize());
document
  .getElementById("win-close")
  ?.addEventListener("click", () => appWindow.close());

// 解析 URL 参数填充标题
const urlParams = new URLSearchParams(window.location.search);
const alias = urlParams.get("alias") || "Unknown Process";
const id = urlParams.get("id");

document.getElementById("process-alias").textContent = alias;
document.title = `${alias} - 控制台输出`;

console.log(`Console window loaded for process ${id} (${alias})`);

// 显示时间戳控制
showTimestampCheckbox.addEventListener("change", (e) => {
  showTimestamp = e.target.checked;
  // 切换所有时间戳的显示状态
  const timestamps = consoleOutput.querySelectorAll(".timestamp");
  timestamps.forEach((ts) => {
    ts.style.display = showTimestamp ? "" : "none";
  });
});

// 自动滚动控制
autoScrollCheckbox.addEventListener("change", (e) => {
  autoScrollEnabled = e.target.checked;
  if (autoScrollEnabled) {
    scrollToBottom();
  }
});

// 辅助函数：滚动到底部
function scrollToBottom() {
  if (consoleOutput) {
    consoleOutput.scrollTop = consoleOutput.scrollHeight;
  }
}

// // 辅助函数：更新行数统计
// function updateLineCount() {
//   if (lineCountEl) {
//     lineCountEl.textContent = lineCount;
//   }
// }

// 功能实现：清空输出
function clearConsoleData() {
  if (consoleOutput) {
    consoleOutput.innerHTML = "";
  }
  consoleLines = [];
  lineCount = 0;
  // updateLineCount();
}
document.getElementById("btn-clear")?.addEventListener("click", () => {
  clearConsoleData();
  isWaitingForStream = false;
});

// 功能实现：复制所有输出
document.getElementById("btn-copy")?.addEventListener("click", async () => {
  try {
    // 提取纯文本内容
    const textContent = consoleLines
      .map((line) => {
        return `[${line.time}] ${line.text}`;
      })
      .join("");

    await writeText(textContent);

    // 简单的复制成功反馈
    const btn = document.getElementById("btn-copy");
    const originalHtml = btn.innerHTML;

    btn.innerHTML =
      '<span class="material-symbols-outlined">check</span> 已复制';
    btn.style.color = "var(--success)";

    setTimeout(() => {
      btn.innerHTML = originalHtml;
      btn.style.color = "";
    }, 2000);
  } catch (err) {
    console.error("复制失败:", err);
  }
});

// 暴露给外部调用的添加输出方法
// time: 字符串时间戳（由外部传入）, text: 输出内容
function appendConsole(time, text) {
  // 保存数据
  consoleLines.push({ time: time, text: text });
  lineCount++;
  // updateLineCount();

  // 创建 DOM 节点
  if (consoleOutput) {
    const row = document.createElement("div");
    row.className = "console-line";

    const timeSpan = document.createElement("span");
    timeSpan.className = "timestamp";
    timeSpan.textContent = `[${time}]`;
    // 根据当前设置决定是否显示时间戳
    timeSpan.style.display = showTimestamp ? "" : "none";

    const textSpan = document.createElement("span");
    textSpan.className = "console-text";
    textSpan.textContent = text;

    row.appendChild(timeSpan);
    row.appendChild(textSpan);

    consoleOutput.appendChild(row);

    // 限制最大行数
    if (consoleOutput.children.length > 2000) {
      consoleOutput.removeChild(consoleOutput.firstChild);
      consoleLines.shift();
    }

    if (autoScrollEnabled) {
      scrollToBottom();
    }
  }
}

// 添加系统消息的辅助函数
function appendSystemMessage(msg) {
  const now = new Date();
  const timeStr =
    now.toLocaleTimeString("zh-CN", { hour12: false }) +
    "." +
    now.getMilliseconds().toString().padStart(3, "0");
  appendConsole(timeStr, msg);
}

async function initConsole(mtId) {
  const checkAndClearWaiting = () => {
    if (isWaitingForStream) {
      clearConsoleData();
      isWaitingForStream = false;
    }
  };
  // 获取实时输出
  const unlisten = await listen(`console_out_stream_${mtId}`, (event) => {
    checkAndClearWaiting();
    appendConsole(event.payload.time, event.payload.msg);
  });
  try {
    // 获取历史缓存
    const historyLogs = await invoke("get_target_console_output", {
      id: parseInt(mtId),
    });
    if (historyLogs && historyLogs.length > 0) {
      checkAndClearWaiting();
    }
    historyLogs.forEach((msg) => appendConsole(msg.time, msg.msg));
  } catch (e) {
    console.error("Failed to fetch history:", e);
  }
}

// 初始化时添加一条准备就绪消息
appendSystemMessage("等待输出流...");
initConsole(id);

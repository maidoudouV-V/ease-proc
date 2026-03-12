const { invoke } = window.__TAURI__.tauri;
const { listen } = window.__TAURI__.event;
const { open, ask } = window.__TAURI__.dialog;
const { appWindow } = window.__TAURI__.window;

import { createApp } from "./petite-vue.es.js";
import { createToastStore } from "./toast.js";

let vueApp;

window.onload = function () {
  appWindow.hide();
  // 禁用右键菜单 (保留开发环境)
  document.addEventListener("contextmenu", (event) => {
    // 判断是否为开发环境：通常开发环境使用 http 协议，生产环境使用 tauri/https 协议
    // 或者判断 hostname 是否为 localhost 且带有端口号
    const isDev = window.location.protocol === "http:";
    if (!isDev) {
      event.preventDefault();
    }
  });
  // 应用窗口控制按钮事件绑定
  document
    .getElementById("titlebar-minimize")
    .addEventListener("click", () => appWindow.minimize());
  document
    .getElementById("titlebar-maximize")
    .addEventListener("click", () => appWindow.toggleMaximize());
  document
    .getElementById("titlebar-close")
    .addEventListener("click", () => appWindow.hide());

  // 窗口状态监听器
  appWindow.onResized(() => {
    updateWindowState();
  });
  // 最大窗口时关闭圆角
  async function updateWindowState() {
    const isMaximized = await appWindow.isMaximized();
    const appContainer = document.querySelector(".app-container");
    if (isMaximized) {
      appContainer.classList.add("maximized");
    } else {
      appContainer.classList.remove("maximized");
    }
  }

  // 初始化VUE数据
  const scope = {
    // 全局状态管理
    activeTab: "dashboard", // 当前激活的标签页: dashboard, monitor-management, performance
    count: 0,
    monitorList: [], // 监控目标列表
    systemInfo: {}, // 系统信息
    loadingStates: {}, // 加载状态
    systemStatus: {
      // 系统状态
      cpu_usage: 0,
      memory_usage: 0,
      download_speed: "0 B/s",
      upload_speed: "0 B/s",
      network_saturation: "0%",
    },
    hasUpdate: false,

    // Toast 方法
    toast: createToastStore(),
    showToast(type, title, message, duration = 3000) {
      this.toast.add(type, title, message, duration);
    },

    // 设置状态
    autoStart: false,
    appVersion: "0.0.0",

    // currentTargetType: "LocalProcess", // 当前监控类型
    dropdownStates: {}, // 下拉菜单状态

    // 日志模态框状态
    showLogModal: false,
    activeLogTab: "info",
    logList: [],
    logRefreshTimer: null,
    async openLogModal(tab = "info") {
      this.activeLogTab = tab;
      this.showLogModal = true;
      try {
        // 调用后端接口
        this.logList = await invoke("get_app_logs", {
          filterType: this.activeLogTab,
        });
      } catch (e) {
        console.error("获取日志失败", e);
        this.showToast("error", "获取日志失败", e);
      }
      this.logRefreshTimer = setInterval(async () => {
        this.logList = await invoke("get_app_logs", {
          filterType: this.activeLogTab,
        });
      }, 1000);
    },

    closeLogModal() {
      if (this.logRefreshTimer) {
        clearInterval(this.logRefreshTimer);
        this.logRefreshTimer = null;
      }
      this.showLogModal = false;
    },

    // 监控模态框状态 (新建/编辑)
    showNewMonitorModal: false,
    isEditMode: false,
    editingMonitorId: null, // 正在编辑的监控ID
    newMonitorForm: {
      target_type: "LocalProcess",
      type_config: {},
      alias: "",
      monitor_enabled: true,
    },
    newProcessConfig: {
      path: "",
      auto_restart: true,
      capture_output: true,
    },
    openNewMonitorModal() {
      this.isEditMode = false;
      this.editingMonitorId = null;

      // 重置表单以供新建
      this.newProcessConfig.path = "";
      this.newProcessConfig.auto_restart = true;
      this.newMonitorForm = {
        target_type: "LocalProcess",
        type_config: this.newProcessConfig,
        alias: "",
        monitor_enabled: true,
      };
      this.showNewMonitorModal = true;
    },

    async openEditMonitorModal(target) {
      const form = await invoke("get_monitor_full_config", { id: target.id });
      this.isEditMode = true;
      this.editingMonitorId = target.id;

      this.newMonitorForm.alias = form.alias;
      this.newMonitorForm.target_type = form.target_type;

      // 填充特定类型的配置
      if (form.target_type === "LocalProcess" && form.type_config) {
        this.newProcessConfig.path = form.type_config.path;
        this.newProcessConfig.auto_restart = form.type_config.auto_restart;
        this.newProcessConfig.capture_output = form.type_config.capture_output;
      }
      this.newMonitorForm.type_config = this.newProcessConfig;
      this.showNewMonitorModal = true;
    },

    closeNewMonitorModal() {
      this.showNewMonitorModal = false;
      // 重置编辑状态
      this.isEditMode = false;
      this.editingMonitorId = null;
    },

    async selectMonitorPath() {
      try {
        const selected = await open({
          multiple: false,
          filters: [
            {
              name: "Executable",
              extensions: ["exe", "bat", "sh"],
            },
            {
              name: "所有类型",
              extensions: ["*"],
            },
          ],
        });
        if (selected) {
          this.newProcessConfig.path = selected;
          // 自动填充名称（如果为空）
          if (!this.newMonitorForm.alias) {
            const fileName = selected.split(/[\\/]/).pop();
            this.newMonitorForm.alias = fileName;
          }
        }
      } catch (error) {
        console.error("选择文件失败:", error);
      }
    },

    async saveMonitor() {
      // 统一处理创建和编辑的逻辑
      const action = this.isEditMode ? "更新" : "创建";
      // 表单验证
      if (!this.newMonitorForm.alias.trim()) {
        this.showToast("error", "验证失败", "请填写监控任务名称");
        return;
      }
      if (
        this.newMonitorForm.target_type === "LocalProcess" &&
        !this.newProcessConfig.path.trim()
      ) {
        this.showToast("error", "验证失败", "请选择或输入启动路径");
        return;
      }

      // 设置按钮为加载状态
      const confirmButton = document.querySelector(
        ".modal-footer .primary-btn",
      );
      if (confirmButton) {
        confirmButton.classList.add("loading");
        confirmButton.disabled = true;
      }

      try {
        if (this.isEditMode) {
          // --- 编辑逻辑 ---
          const updateForm = {
            ...this.newMonitorForm,
            id: this.editingMonitorId,
          };
          await invoke("update_monitor_target", {
            updateTargetForm: updateForm,
          });
          this.showToast("success", "更新成功", "监控任务已成功更新");
          let enabled = this.monitorList.find(
            (item) => item.id === this.editingMonitorId,
          )?.monitor_enabled;
          if (enabled) {
            this.loadingStates[this.editingMonitorId] = true;
          }
        } else {
          // --- 新建逻辑 ---
          await invoke("add_monitor_target", {
            newTargetForm: this.newMonitorForm,
          });
          this.showToast("success", "创建成功", "监控任务已成功创建");
        }

        this.closeNewMonitorModal(); // 关闭模态框
        this.refreshMonitorList(); // 刷新列表
      } catch (error) {
        console.error(`${action}监控失败:`, error);
        this.showToast("error", `${action}失败`, error || "未知错误，请重试");
      } finally {
        // 恢复按钮状态
        if (confirmButton) {
          confirmButton.classList.remove("loading");
          confirmButton.disabled = false;
        }
      }
    },
    // 启动进程
    async startProcess(id, event) {
      this.loadingStates[id] = true;
      try {
        await invoke("send_control_signal", {
          id: parseInt(id),
          signal: "start",
        });
        this.showToast("info", "启动", "正在启动进程");
      } catch (error) {
        console.error("启动程序失败:", error);
        this.showToast(
          "error",
          "启动失败",
          error.message || "启动监控任务失败",
        );
      }
    },
    // 停止进程
    async stopProcess(id, event) {
      this.loadingStates[id] = true;
      try {
        await invoke("send_control_signal", {
          id: parseInt(id),
          signal: "stop",
        });
        this.showToast("info", "停止", "正在停止进程");
      } catch (error) {
        console.error("停止程序失败:", error);
        this.showToast(
          "error",
          "停止失败",
          error.message || "停止监控任务失败",
        );
      }
    },
    // 刷新监控列表
    async refreshMonitorList() {
      try {
        const targets = await invoke("refresh_monitor_targets");
        this.monitorList = targets;
        console.log("refreshMonitorList", targets);
      } catch (error) {
        console.error("刷新监控列表失败:", error);
      }
    },
    // 系统信息管理
    async getSystemInfo() {
      // 获取系统信息
      try {
        this.systemInfo = await invoke("get_system_info");
        // 设置启动时间并开始更新（bootTime 是从 Rust 后端获取的秒级时间戳）
        bootTime = this.systemInfo.start_time;
        this.updateUptime(); // 立即更新一次
        setInterval(this.updateUptime, 1000); // 每秒更新
        console.log("系统信息：", this.systemInfo);
      } catch (error) {
        console.error("获取系统信息出错：", error);
      }
    },
    updateUptime() {
      if (bootTime) {
        const uptime = Date.now() - bootTime * 1000;
        this.systemInfo.start_time = formatUptime(uptime);
      }
    },
    // 监控目标管理
    // 切换监控启用状态
    async toggleMonitorEnabled(target, event) {
      this.loadingStates[target.id] = true;
      const newState = event.target.checked;
      try {
        if (newState) {
          await invoke("enable_monitor", { id: parseInt(target.id) });
        } else {
          await invoke("disable_monitor", { id: parseInt(target.id) });
          this.showToast("info", "关闭监控", `已停止监控: ${target.alias}`);
          this.loadingStates[target.id] = false;
        }
        target.monitor_enabled = newState; // 更新前端状态
        this.refreshMonitorList();
      } catch (err) {
        event.target.checked = !newState; // 失败时回滚开关状态
        this.showToast("error", "操作失败", err.toString());
        this.loadingStates[target.id] = true;
      }
    },
    async deleteProgram(id) {
      const confirmed = await ask("确定要删除这个监控任务吗？", {
        title: "确认删除",
        type: "warning",
      });

      if (!confirmed) {
        return;
      }
      // 执行删除操作
      try {
        // 确保id是数字类型
        await invoke("delete_monitor_target", { id: parseInt(id) });
        this.showToast("success", "删除成功", "监控任务已删除");
        this.refreshMonitorList();
      } catch (error) {
        console.error("删除监控目标失败:", error);
        this.showToast(
          "error",
          "删除失败",
          error.message || "删除监控任务失败",
        );
      }
    },

    get enabledMonitorCount() {
      return this.monitorList.filter((item) => item.monitor_enabled).length;
    },
    // 开启自启动
    async enableAutoStart() {
      try {
        await invoke("plugin:autostart|enable");
        this.autoStart = true;
        console.log("自启动已开启");
      } catch (e) {
        console.error("开启失败", e);
        this.autoStart = false; // 失败回滚
      }
    },
    // 关闭自启动
    async disableAutoStart() {
      try {
        await invoke("plugin:autostart|disable");
        this.autoStart = false;
        console.log("自启动已关闭");
      } catch (e) {
        console.error("关闭失败", e);
        this.autoStart = true; // 失败回滚
      }
    },

    // 设置初始化
    async initSettings() {
      if (window.__TAURI__) {
        try {
          // 获取自启状态
          this.autoStart = await invoke("plugin:autostart|is_enabled");
          // 获取版本号
          if (window.__TAURI__.app) {
            this.appVersion = await window.__TAURI__.app.getVersion();
          } else {
            this.appVersion = "Dev Build";
          }
        } catch (e) {
          console.error("Settings init error:", e);
        }
      }
    },

    // 打开GitHub
    openGithub() {
      if (window.__TAURI__?.shell) {
        window.__TAURI__.shell.open(
          "https://github.com/maidoudouV-V/ease-proc",
        );
      } else {
        this.showToast("warning", "无法打开链接", "Shell 模块未加载");
      }
    },

    // 打开控制台窗口
    async openConsoleWindow(target) {
      if (window.__TAURI__?.window) {
        try {
          const { WebviewWindow } = window.__TAURI__.window;

          const label = `console-${target.id}`;
          // 检查窗口是否已存在
          const existingWin = await WebviewWindow.getByLabel(label);
          if (existingWin) {
            await existingWin.unminimize();
            await existingWin.setFocus();
            return;
          }
          console.log(target);
          // 创建新窗口
          new WebviewWindow(label, {
            url: `console.html?id=${target.id}&alias=${encodeURIComponent(target.alias)}&pid=${target.performance_record.pid}`,
            title: `${target.alias} - 控制台输出`,
            width: 800,
            height: 600,
            resizable: true,
            decorations: false, // 无边框窗口，使用自定义标题栏
            transparent: false,
          });
        } catch (e) {
          console.error("Open console window failed:", e);
          this.showToast(
            "error",
            "打开失败",
            "无法创建控制台窗口: " + e.message,
          );
        }
      } else {
        console.warn("Tauri window API not available");
      }
    },
    // 打开设置页面
    openSettings() {
      this.activeTab = "settings";
      this.checkUpdate();
    },
    // 打开监控进程文件夹
    openFolder(id) {
      invoke("open_app_folder", { id: id });
    },
    // 重置数据
    async reset_database() {
      const confirmed = await ask(
        "确定要重置所有数据？此操作不可逆！\n程序将在重置后自动退出。",
        {
          title: "重要提示",
          type: "warning",
        },
      );
      if (!confirmed) {
        return;
      }
      await invoke("reset_database");
    },
    // 更新版本
    async updateSelf() {
      await invoke("update_self");
      this.showToast("info", "正在更新", "稍后将自动重启...");
    },
    // 检查更新
    async checkUpdate() {
      this.hasUpdate = await invoke("check_update_self");
    },
    // 启动时调用
    async mounted() {
      this.initSettings();
      this.getSystemInfo();
      this.refreshMonitorList();
      listen("monitor-info-update", (event) => {
        let monitorList = [];
        for (let monitorTarget of event.payload) {
          if (monitorTarget.target_type === "LocalHost") {
            this.systemStatus.cpu_usage = Math.round(
              monitorTarget.performance_record.cpu_usage,
            );
            this.systemStatus.memory_usage =
              monitorTarget.performance_record.memory_usage;
            this.systemStatus.download_speed =
              monitorTarget.performance_record.download_speed;
            this.systemStatus.upload_speed =
              monitorTarget.performance_record.upload_speed;
            this.systemStatus.network_saturation =
              monitorTarget.performance_record.network_saturation;
            continue;
          }
          if (monitorTarget.target_type === "LocalProcess") {
            monitorList.push(monitorTarget);
          }
        }
        this.monitorList = monitorList;
      });
      // 响应后端事件
      listen("status_signal", async (event) => {
        console.log("Received status_signal:", event.payload);
        this.refreshMonitorList();
        const { mt_id, target_type, signal, message } = event.payload;
        let target = this.monitorList.find((t) => t.id === mt_id);
        if (signal === "enable") {
          this.showToast(
            "success",
            `启动监控`,
            `已启动${target.alias}监控任务`,
          );
          this.loadingStates[mt_id] = false;
          return;
        }
        switch (target_type) {
          case "LocalProcess": {
            if (signal === "error") {
              this.showToast("error", "监控启动错误", `${message}`);
              this.loadingStates[mt_id] = false;
              this.toggleMonitorEnabled(target, { target: { checked: false } });
            } else if (signal === "start") {
              this.showToast("success", `启动成功`, `${message}`);
              this.loadingStates[mt_id] = false;
            } else if (signal === "stop") {
              this.showToast("info", `停止运行`, `${message}`);
              this.loadingStates[mt_id] = false;
            } else if (signal === "refresh") {
              this.refreshMonitorList();
            }
            break;
          }
          default:
        }
      });
      let resetHasUpdateTimer = null;
      listen("hasUpdate", (event) => {
        this.hasUpdate = true;
        clearTimeout(resetHasUpdateTimer);
        resetHasUpdateTimer = setTimeout(() => {
          this.hasUpdate = false;
        }, 3601 * 1000);
      });
      // 显示并聚焦窗口
      await appWindow.show();
      await appWindow.setFocus();
    },
  };
  // 创建VUE应用
  vueApp = createApp(scope);
  vueApp.mount();
};

// 格式化时间
let bootTime = null;
function formatUptime(milliseconds) {
  const seconds = Math.floor(milliseconds / 1000);
  const days = Math.floor(seconds / (24 * 3600));
  const hours = Math.floor((seconds % (24 * 3600)) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;

  const uptimeElement = document.getElementById("uptime-value");

  if (days > 0) {
    if (uptimeElement) {
      uptimeElement.classList.add("small-text");
    }
    return `${days}天 ${hours}小时 ${minutes}分钟 ${secs}秒`;
  } else {
    if (uptimeElement) {
      uptimeElement.classList.remove("small-text");
    }
    return `${hours}小时 ${minutes}分钟 ${secs}秒`;
  }
}

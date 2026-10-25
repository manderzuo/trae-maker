# 托盘点击不显示窗口（WebView2 数据目录被抢占）

## 现象

- 进程在跑、托盘图标在；
- 左键点托盘图标弹出的是**右键菜单**，窗口始终不出现；
- 日志里反复出现 `托盘左键点击：显示窗口`，但屏幕上没有任何窗口；
- 关掉重开、换回旧版本二进制都一样。

## 排查过程与关键证据

1. 枚举该进程全部顶层窗口，只有 `tray_icon_app`、`com.aiwork.assistant-sic`（单实例辅助窗口，15×15）、
   `Tao Thread Event Target`、`IME` —— **主窗口根本不存在**。
   注意：`Get-Process.MainWindowHandle` 会指向那个 15×15 的单实例窗口，用它判断窗口尺寸会被误导。
2. 100ms 高频轮询启动过程：`t=300ms` 出现 1 个窗口，`t=400ms` 又变成 0 —— 窗口**建了随即被销毁**，
   且没有 `CloseRequested` 日志（不是我们的关闭逻辑）。
3. 该进程**没有任何子进程**，`msedgewebview2.exe` 也不是它的 —— WebView2 从未初始化成功。
4. 决定性实验：
   - 用 `WEBVIEW2_USER_DATA_FOLDER=<全新空目录>` 启动 → 窗口正常出现、WebView2 子进程 2 个；
   - 查环境变量：**用户级** `WEBVIEW2_USER_DATA_FOLDER = E:\AIWORK\webview2`。
     它作用于本机所有 WebView2 宿主；`GameViewer.exe` 等程序也在用同一个目录并持有独占锁，
     我们的应用因此建不出 WebView2 环境，Tauri 随后无法建出主窗口，但进程不退出、也不报错。

## 修复（`src-tauri/src/main.rs`）

1. `isolate_webview2_data_dir()`：在 `main()` 最开头把本进程的 `WEBVIEW2_USER_DATA_FOLDER`
   强制改到私有目录 `%LOCALAPPDATA%\com.aiwork.assistant\EBWebView`，不再受全局共享目录影响。
   （必须在创建任何窗口之前执行，环境变量是进程级的。）
2. `TrayIconBuilder::show_menu_on_left_click(false)`：Tauri 2 默认左键弹菜单，不显式关掉时
   左键会被菜单消费，用户看到的就是「点一下就弹右键菜单」。
3. 显示窗口时补 `unminimize()`：最小化状态下 `show()` 不会还原。
4. 记录主窗口 `CloseRequested` / `Destroyed` 事件，便于下次区分「正常关闭」与「被销毁」。

## 验证

| 检查项 | 结果 |
|---|---|
| 全局 `WEBVIEW2_USER_DATA_FOLDER=E:\AIWORK\webview2` 仍存在时启动 | 窗口正常显示 |
| 该进程的 WebView2 子进程 | 2 个 |
| `%LOCALAPPDATA%\com.aiwork.assistant\EBWebView` | 161 个文件（私有目录已启用） |
| 运行中 exe 哈希 == 部署件 == 构建产物 | `ABC6C9BA…FCB8A7` |

## 遗留建议

`WEBVIEW2_USER_DATA_FOLDER` 设为全局变量本身是个隐患：任何两个 WebView2 应用共用它，
后启动的那个就会抢不到锁。本应用已自带隔离，可不再依赖该变量；如无特殊用途，
建议后续把该用户级变量移除，避免其它 WebView2 程序重蹈覆辙。

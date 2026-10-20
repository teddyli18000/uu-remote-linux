# Ubuntu 18.04 X11 与本机终端视图

本分支保留 Ubuntu 18.04 用户空间、系统 GLib/GTK/FFmpeg。默认 X11 被控端是目标；`inproc-av`、Wayland/portal、KMS 和实验 Linux 控制端不属于此兼容结论。

## 构建

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config curl ca-certificates \
  libx11-dev libxext-dev libxtst-dev libxfixes-dev libgtk-3-dev libvte-2.91-dev ffmpeg
# 使用 rustup 安装固定工具链；只改变当前用户的工具链，不升级系统组件。
rustup toolchain install 1.99.1 --profile minimal
cargo +1.99.1 test --locked
cargo +1.99.1 build --release --locked
ldd target/release/uuremote-server
```

图形登录依赖 `libgtk-3-0`；新增本机终端视图运行库为 `libvte-2.91-0`（由系统包管理器解析其依赖）。构建时安装开发包 `libvte-2.91-dev`，无需额外启动守护进程或安装 SSH/tmux。保留原系统 FFmpeg 3.4；初验指定 `UU_VIDEO_ENCODER=cpu UU_VIDEO_CODEC=h264`。

## 终端行为

沿用现有 session registry 和单个 PTY reader。远程 Terminal 的输入仅写入原 PTY；PTY 输出送入原远程通道及本机 VTE 窗口。渲染进程由同一二进制启动，只渲染字节，不启动另一份 shell。关闭回显的输入不会被额外显示。

远端决定 PTY 行列数，本机窗口变化不修改远端的 shell 尺寸。普通命令结束、DETACH 和连接断开后 shell/窗口保留；重新 OPEN 原 session 时复用。按用户更新后的要求，`main` 中主动关闭本机终端窗口会结束对应会话；PTY reader 回收后发 EXIT、移除 registry 条目并通知远端刷新列表，其他独立会话保留。渲染器故障或队列满时仅关闭视图，原 shell 保留，重新 OPEN 可恢复视图。`v0.2.12-aic.1` 仍采用旧的关窗口保留 shell 行为。CLOSE、KILL、显式 `exit` 保留既有结束语义；已退出 session 不会冒充活 session。持久性不跨服务/系统重启。

无可用图形显示或构建时缺少 VTE 时，原远程终端仍可用，日志会报告视图不可用。本机渲染滞后超过队列容量时关闭该视图，避免阻塞远程输出；重开时使用现有有界 scrollback。

主动关闭采用渲染器到主机的独立状态字节 6，由 GTK `delete-event` 发出；EOF、渲染器被清理或故障不会发送此请求。主机结束对应 shell 及独立前台进程组，沿用已有 EXIT/列表通知流程。Ubuntu 18.04 release 测试 192 passed、0 failed、6 ignored；新增真实 PTY 检查覆盖关闭时前台命令仍在运行及另一个会话保持存活，独立 Xvfb 上实际发送 `WM_DELETE_WINDOW` 也确认状态 6 和渲染器正常退出。

`484f7df` 在 WSL 上完成 Windows 客户端验收：先保持桌面连接，创建两个终端，再在 WSL 桌面点 × 关闭其中一个本机窗口。用户确认对应条目消失，另一个仍正常。日志在 UTC 2026-10-19 17:32:24 显示本机关闭请求、对应 shell 退出、变化通知、随后 LIST 数量从 2 变成 1；另一个会话继续收到 RESIZE。当时仅部署到 WSL；后续车端更新见下方记录，`v0.2.12-aic.1` 发布包仍保留旧行为。

## 验证记录

源码基线 `949acaac30f2319a0d90b7f5326657936248146e`，兼容修复 `ef33c21`，终端实现及本地验收 `db88199`。2026-10-19 一次性 WSL 实测：Ubuntu 18.04.6、glibc 2.27、GTK 3.22.30、GLib 2.56.4、VTE 0.52.2、FFmpeg 3.4.11、GCC 7.5.0、libstdc++6 8.4.0、Rust 1.99.1。

| 项目 | 实际结果 |
| --- | --- |
| 默认 X11 release / tests | `cargo build --release --locked` 通过；`cargo test --release --locked`：190 passed、0 failed、6 ignored |
| 动态库与 ABI | `ldd` 无缺失，GTK/VTE 均链接；最高直接 GLIBC_2.25，无直接 GLIBCXX 需求；GTK 登录与 VTE C 符号均存在 |
| 图形登录 | Bionic GTK 实际绘制；用户扫码确认，临时主机向云端同步为 ready。二维码和凭据没有导出 |
| 桌面编码 | Xvfb 1440×900，系统 FFmpeg 3.4 / libx264，5 帧、24506 bytes、首帧 IDR |
| 键鼠与剪贴板 | XTest→GTK motion/click/key 实际生效；双向 Unicode 剪贴板各 38 bytes |
| 音频 | 独立 Pulse null sink monitor→Opus，3 秒 86 包、399 bytes；这是静音虚拟源，不证明物理设备或客户端播放 |
| 同会话终端 | 真实 TerminalConn→原 PTY→VTE：中文/ANSI、120 行连续输出顺序及唯一性、stderr、Ctrl-C、关闭回显、远端 80×24→100×30→80×24、本机窗口缩放均通过 |
| 生命周期与隔离 | 两个 PTY/session/window 独立；DETACH、连接通道释放、重连、关闭/重开视图保留 PID/cwd/env；无害运行命令断线后完成；exit/CLOSE/KILL 与 LIST 状态通过 |
| 文件与端口映射 | 已有临时文件上传写块/确认、目录与压缩列表测试通过；新增 loopback TCP 真实 SYN/DATA 双向收发/FIN EOF 通过。未经过官方客户端网络通道 |
| 官方 Windows 客户端 | 原始 WSL 验收未完成官方客户端验证。后续车端日志确认桌面与首次创建终端成功，但非空会话列表触发客户端初始化失败；见下方修复记录 |

本机终端视图的实际 X11 截图（全部是合成验收内容）：

![Ubuntu 18.04 VTE terminal](validation/ubuntu-18.04-terminal.png)

本地验收命令（先提供专用 Xvfb 与轻量窗口管理器；测试会注入键鼠，拒绝普通桌面）：

```sh
export DISPLAY=:99 GDK_BACKEND=x11 LC_ALL=C.UTF-8 LANG=C.UTF-8
UU_DESKTOP_BACKEND=x11 target/release/uuremote-server terminal-acceptance
UU_X11_ACCEPTANCE=1 UU_DESKTOP_BACKEND=x11 cargo test --release --locked \
  --bin uuremote-server x11_acceptance::actual_x11_input_and_clipboard -- --ignored --nocapture
```

UTF-8 locale 是 GTK Unicode 剪贴板探针的前提；`LC_ALL=C` 会使该探针失败。`terminal-acceptance` 创建真实临时 shell，结束后留下最终画面供查看；不登录云端。CI 的 Bionic job 在 `ubuntu:18.04` 用户空间执行构建、单测和 ABI 检查，上传二进制 artifact；本地 Xvfb 验收与官方客户端验收分别记录。

### Windows 非空终端列表修复（2026-10-19）

车端旧版日志：空列表后客户端发送 OPEN 并正常使用；保留一个会话后，客户端收到 LIST_RESP 便立即报断开，不再发送 OPEN。重启主机清空会话后首次创建恢复，说明首次创建成功不等于重连成功。

只读检查官方 Windows UU 4.42.0.2770 的 `terminal_management_model.cpp` 解析路径：每条记录都必须包含整数 `last_active_ms`（读取地址 `0x1410a209a`）和 `created_at_ms`，缺字段走解析失败分支。上游沿用的列表应答缺 `last_active_ms`；此次 VTE 改动没有修改该应答。现已补字段，创建时初始化，PTY 输出、输入、OPEN/RESIZE/SIGNAL/DETACH 时更新；不记录终端内容。

验证：新增必需字段断言在旧实现失败；修复后 Ubuntu 18.04 release 单测 190 passed、0 failed、6 ignored，默认 X11 release 构建通过。专用 Xvfb/Openbox 下真实 PTY/VTE 验收通过，包括非空列表字段校验、DETACH/重连、通道释放、多会话、关闭/重开视图、关闭回显及 exit/CLOSE/KILL。启动验收前等待窗口管理器就绪，并设置 `NO_AT_BRIDGE=1`，避免 WSL 图形环境干扰。

同日经用户授权将修复 `ec23703` 替换到车端独立 release 目录，保留旧版及回退路径；未改变系统包或自启配置。安装文件与运行中进程 SHA256 均为 `5cc836bf22d1e5cab064074c922b5f15f91cd2aa3f5f2fd1d11f4e4cabe9f150`，车端 `ldd` 无缺失，重启后服务 enabled/running、云端 ready、SSH 正常。官方 Windows 客户端的保留会话重连仍待用户验收，不能由服务在线状态替代。

### Windows 会话管理终止修复（2026-10-19）

上一项修复安装后，车端收到 Windows 的 KILL_REQ（type 11），header 与前一条 LIST_REQ 相同，为管理请求编号，JSON payload 为 16 字节；当时实际活会话只有 `1`、`2`。原实现优先将非零 header 当目标，因此用户多次点击“终止”都未结束 shell。先前验收只覆盖空 payload、header 直接指定目标的方式，未覆盖 Windows 的管理请求方式。

只读核对当前官方 Windows UU 4.43.0.3607：`terminal_management_model` 的 KILL_REQ 构造路径（`0x1410bcfc6` 至 `0x1410bd0ea`）将 `session_id` 放入 JSON；KILL_RESP 分支（`0x1410bf2de`）使用 `code/message` 解析器（`0x1410bf840`）。修复为非空 payload 严格解析非零 u32 目标，非法或溢出时直接拒绝，不回退到 header；空 payload 保留旧客户端方式。所有 KILL_RESP 回显请求 header。

定向回归使用两个真实 PTY，让请求编号恰好等于另一活会话编号，检查只结束 JSON 指定的目标、目标被回收且从列表移除、另一 shell 存活，并覆盖畸形/缺失/零/负数/浮点/字符串/溢出 ID 和旧方式。新增测试在旧实现失败，原因是目标 shell 未被终止。`terminal-acceptance` 的一条 KILL 路径也改为 Windows 请求格式，另一条仍验收旧格式。

修复后 Ubuntu 18.04 release 单测 191 passed、0 failed、6 ignored，默认 X11 release 构建通过。真实 PTY/VTE 验收通过，包括 Windows 格式终止目标后另一会话继续显示，以及列表回收、断线重连、窗口恢复、中文/ANSI、尺寸锁定、关闭回显和 exit/CLOSE/旧格式 KILL。构建直接引用的最高 glibc 版本为 GLIBC_2.25，GTK/VTE 运行库解析正常；官方客户端点击“终止”仍需车端验收。

同日将修复 `fcbc353` 安装到车端独立 release 目录，保留上一版 `ec23703` 供回退，沿用现有配置与自启服务。安装文件和运行中进程的 SHA256 均为 `b14bef91fbeaae498fe15f3efcc9b81619a6bcb1a929cb728c13a03d96d2a902`，车端 `ldd` 无缺失；重启后服务 enabled/running、NRestarts=0、云端 ready、SSH 正常。尚待用户用官方 Windows 客户端验收目标终止及另一会话继续工作。

后续官方 Windows 实测未通过：用户反馈点击“终止”后列表条目仍保留。对应车端首次 KILL_REQ 后目标 shell 立即退出，之后客户端重复发送同一目标的 KILL_REQ；当次检查目标进程已不在，UU 服务仍 running、NRestarts=0，未发现 BINARY 发送失败日志。这证明进程终止生效，但不能证明客户端处理了 KILL_RESP，列表残留原因尚未确定。`8721eeb` 增加管理应答入队/成功交给 WebRTC 的元数据日志（类型、编号、字节数、入队布尔值，无终端内容），Ubuntu 18.04 release 单测仍为 191 passed、0 failed、6 ignored，release 构建通过。该诊断版上传时车端 SSH 连续超时，尚未安装；车端最后确认运行的仍为 `fcbc353`。

## WSL Windows 会话列表刷新诊断

`v0.2.12-aic.1` 在独立 WSL 测试设备上复现：先打开桌面，再打开终端，两者可同时使用，终端复用了桌面连接。初次 LIST 返回空数组后，创建两个会话、DETACH 一个会话均未收到新的 LIST；用户界面一直显示空列表。这一结果不代表独立终端连接与桌面连接的冲突已经修复。

依据 [uu-cli PR #1](https://github.com/teddyli18000/uu-cli/pull/1) 的 `4193a3d` 协议说明，测试实现增加空 `Message` 字段 28 `TerminalSessionChanged`：在 OPEN/重附着、DETACH、实际退出并移除 registry 条目后广播变化。每个 BINARY 通道订阅变化，通过已有顺序应答队列发出通知；关闭通道时取消订阅任务。通知不携带终端内容，KILL 应答及连接替换逻辑不变。

`92230fc` 已在独立 WSL 设备、官方 Windows 客户端的桌面复用连接上验收：用户确认新建/关闭窗口后列表开始更新，点击终止也正常。日志显示 OPEN 和 DETACH 后均先通知、再收到 LIST；KILL_RESP 发出后 shell 退出，移除条目并通知，客户端再发 LIST，返回数量由 1 变成 0。这验证了此路径上的 BINARY 通道路由及列表刷新；原先 shell 已被终止而界面残留的问题，缺失环节是变化通知。

测试构建在 Ubuntu 18.04 上通过 release 测试（191 passed、0 failed、6 ignored）和构建，`92230fc` 的 GitHub CI 也通过。WSL 运行二进制 SHA256 为 `80a7ec9bfba55bb70edc9e177b0311754aa8595c8342a88d038d9943c547b628`。终止后再次新建、独立终端与桌面连接切换仍待验收；本次仅在 WSL 重启运行，车端及 `v0.2.12-aic.1` 发布包均未更新。

[uu-cli PR #2](https://github.com/teddyli18000/uu-cli/pull/2) 的 `809b89d` 后续分析提出通知应走 CONTROL，同时将路由依据标记为推断。本项目上述实测证明 BINARY 通知后 Windows 确实重新发送 LIST 并更新界面，因此不能仅据该推断迁移已工作的路由；尚未验证的独立连接路径应单独核对。

### 先开终端再开桌面的缺口

WSL 的 `484f7df` 在先开终端的连接上收到 SimpleAction 4 `capture_desktop`，却记录为未处理；同一运行期间未收到新的 `capture_type=1` offer。原代码仅在建连时 `desktop=true` 才启动桌面编码、创建输入租约和桌面生命周期任务，后续 RPC case 15 则只回成功。这能解释该路径为何没有画面；没有证据表明本次本地窗口关闭改动修改了连接逻辑。

这次终端 offer 已包含 audio `sendrecv` 和 video `recvonly`，应答保留媒体段，视频轨道确认 `h264=true`。`main` 现按后续 action 4 请求，由 room loop 仲裁同一 peer 的桌面能力，维护输入所有权并启动一次现有音视频、初始化、剪贴板及光标路径。音频 sender 和可用编码在初次 SDP 时预先协商，媒体采集等桌面激活后才开始；未升级的工具连接不抢输入。过期请求在关闭旧桌面前拒绝，headless 或无可用采集/Portal manager 时拒绝升级，Portal 继续沿用授权和锁屏门控。

Ubuntu 18.04 release 测试 193 passed、0 failed、6 ignored，构建通过。新增真实 local SCTP 测试覆盖 action 4 请求前原输入所有者不变、仲裁后目标获得租约、文件连接继续应答及重复激活幂等；该测试不包含实际媒体画面。

`5eeb7d9` 已完成 WSL 的 Windows 界面验收：从设备列表先开终端并创建会话，再保持连接打开桌面，用户确认画面、鼠标与原 Windows 终端命令输入正常。初报的键盘无反应仅发生在本机 `UU Remote Terminal` 可视化窗口；该窗口由 `vte_terminal_set_input_enabled(..., FALSE)` 明确禁用输入，属于既有只读约定，不是桌面升级后丢失键盘事件。日志也确认收到桌面键盘事件且 X11 注入返回成功；本次未另行验证普通桌面输入框。

运行中二进制 SHA256 为 `36de7df45db7f69a321d8ebbb48b983dfecca6d45cbba518da9fd5ca736cc936`，对应提交的 GitHub CI 通过。此前桌面优先的路径已由用户确认桌面与终端同时正常。这些结果仍仅覆盖 WSL X11、CPU H.264、禁用音频的测试设置；车端和 `aic.1` 发布包未更新。

## 车端部署步骤

### 2026-10-20：覆盖安装 v0.2.12-aic.2

经用户授权，将 GitHub Release 的 `uu-remote-linux-v0.2.12-aic.2-x86_64-ubuntu18.04.tar.gz` 安装到车端。安装包 SHA256 为 `70adc3909a08a4c7b05fe544abe071e505bd62215d6c1d99488d79acc8394cab`，与发布校验文件及 GitHub asset digest 一致。二进制 SHA256 为 `5997767677927f3fa39e5d10d0c24431e2b8f6798a2589855ded6aeb4f62f635`，与运行进程一致。

新文件经 `ldd` 和 `help` 检查后，原子覆盖 `~/.local/bin/uuremote-server`，重启原有 `uuremote-native.service`。旧版 `~/.local/share/uuremote/releases/8721eeb/uuremote-server` 保留供回退。登录配置校验前后一致，未更改服务配置、系统依赖或 ROS。部署后确认服务 enabled/active/running、NRestarts=0、云端 ready、SSH 正常，部署以来 ERROR 级日志为 0。尚未完成此次车端更新后的官方客户端交互验收；WSL 验收不替代实车结果。

### 2026-10-20：覆盖安装 v0.2.12-aic.3

经用户授权，安装 GitHub Release 的 Ubuntu 18.04 包；安装包 SHA256 `65fe3a06785eff1349e2cfed728e1ad18aa821b2479de16e78e66d64b3eba5c8` 与发布校验文件和 asset digest 一致，运行进程 SHA256 为 `a30cfff40e342d5a3e25d24e1b0031a468207da06cb46264e50c85b4f4751547`。`ldd` 无缺失，`help` 可运行。用户在车端补装 `xvfb`、`openbox`，部署前确认 Xvfb、xauth、xdpyinfo、dbus-run-session、openbox-session 均可用；用户 linger 已为 yes。

旧 aic.2 二进制及原服务定义保留在 `~/.local/share/uuremote/releases/v0.2.12-aic.2/`。原子覆盖 `~/.local/bin/uuremote-server`，新增 `~/.config/systemd/user/uuremote-native.service.d/20-auto-virtual-display.conf`，清空旧的实体 Xauthority/`:0` 启动前检查，交由 aic.3 自行选择物理或虚拟显示。沿用登录、编码和其他服务配置；登录配置校验前后一致。若回退 aic.2，须同时移除此 drop-in 并执行用户服务 daemon-reload。

UTC 11:38:37 确认服务 enabled/active/running、NRestarts=0、云端 CONNECTED/ready、当前 X11 可访问；SSH 与 SDAutoLogin 服务正常，部署后的 ERROR 级日志为 0。本次启动时 DP-1 为 connected，未人为拔屏或重启整车；无屏开机、实际热插拔及官方客户端交互尚待现场验收。本机临时安装包已清理，未升级系统、ROS 或显卡配置。

### 操作步骤

1. 在车端只读核对当前会话、既有服务和二进制路径；不要硬编码其它机器的 DISPLAY/XAUTHORITY。保留 Ubuntu、glibc、ROS、Python、FFmpeg 和 NoMachine。
2. 获得部署授权后，安装缺少的系统运行库（尤其 `libvte-2.91-0`）。先把本次 Bionic 构建的二进制传到临时路径，运行 `ldd`，确认无缺失库。
3. 在没有远程任务时备份现有二进制，将新文件复制为目标同目录临时文件，`chmod 755` 后 `mv -f` 原子替换。重启现有用户服务会结束正在运行的 UU session，应选择空闲时段。
4. 沿用该用户真实 X11 会话的 DISPLAY、XAUTHORITY 和用户总线。保留 GTK 登录，通过官方 Windows UU 客户端验证桌面、键鼠、声音、剪贴板、文件、Terminal 和端口映射。
5. Terminal 验收使用无敏感内容的变量、工作目录和 shell PID，检查断线/重连、多 session、Ctrl-C、关闭回显、关闭本机窗口、显式 exit/KILL；不把本地探针当作官方客户端验收。

WSL 结果不能证明车端 Intel 硬件编码、真实音频设备、GNOME/Xauthority、开机启动和机器人依赖；这些需要授权后上车验证。

## 一次性环境

2026-10-20，按用户要求已注销 `uu-bionic-test`，其 `F:\Virtual\uu-bionic-test` 磁盘及发行版内测试配置一并销毁。本次生成的 `F:\Virtual` 下 UU 下载、输出、旧发布包、脚本和日志，以及项目 `target/` 缓存均已清理；保留项目源码、已提交的验收图片、用户原始需求文件和其他发行版/虚拟机。以下为当时环境的重建参考，不表示该发行版仍存在。`v0.2.12-aic.2` 发布包改由 GitHub Release 工作流从 tag 构建。

本机新建发行版名 `uu-bionic-test`，安装目录 `F:\Virtual\uu-bionic-test`。未改默认发行版、现有发行版或全局 `.wslconfig`。

Rootfs 来源：[Canonical Bionic core archive](https://partner-images.canonical.com/core/bionic/current/)，文件 `ubuntu-bionic-core-cloudimg-amd64-root.tar.gz`，与同目录 SHA256SUMS 校验一致：

```
d44096a712fee7dc69ea0ad67d90e1364622488cdd58b1407fac135dc942a8d2
```

使用微软支持的 [wsl --import](https://learn.microsoft.com/en-us/windows/wsl/basic-commands#import-a-distribution) 导入为 WSL2。源码和构建位于发行版 Linux 文件系统 `/root/uu-remote-linux`。

交付物导出后可按需仅销毁此发行版（本任务不自动执行）：

```powershell
wsl --terminate uu-bionic-test
wsl --unregister uu-bionic-test
```

注销会永久删除该临时发行版及其中的进程/文件。不要把命令中的名称改成已有发行版。



### 原桌面补屏修正（aic.4）

aic.3 的自动 Xvfb 是独立 Openbox 会话，不能保留原 GNOME 窗口。aic.4 尚未发布；当前开发路线是通过固定路径 `/usr/local/libexec/uuremote-display-helper`，在所有 DRM 连接器均报告明确断开后，运行时强制现有唯一 i915 DRM card 上端口号最小的 HDMI-A 连接器。它不安装、替换或切换显卡驱动，不修改 Xorg/GDM 配置，也不重启图形会话。helper 预期由发行包安装为 root 所有、模式 4750，并限制到本地 GNOME 服务用户的组；网络服务进程本身保持非 root。不能使用 DP，未知状态、多 i915 card 或没有 HDMI 时不得强制连接。内核强制状态不保证无屏 HDMI 实际扫描出可捕获图像。可选 root oneshot 可在 `display-manager.service` 前 force HDMI；它不设置 GDM 自动登录，用户会话仍依赖机器上已有的自动登录设置。具体前提、命令、回滚及验收见[同会话补屏说明](../contrib/virtual-desktop/SAME_SESSION.md)。

不要将旧的 DISPLAY=:99 或 Xvfb 服务当作原桌面配置；服务应使用实际 GNOME Xorg DISPLAY/XAUTHORITY。冷启动需要启用可选 oneshot，在 GDM 前准备 HDMI；GDM 仍必须沿用现有配置启动原用户图形会话，helper 不会配置自动登录。目标机（Linux 5.4.0-150、i915）已经通过临时 HDMI force-on 与三分钟原桌面连接实验：HDMI 拔掉时，官方 UU 可完整显示原 GNOME，并正常拖动窗口、键盘输入，实验后可恢复重连。新版自动热拔屏接管与无屏冷启动仍待验收；dummy CI 不代表硬件结果。现有桌面连接设计为热拔屏后继续工作、无需用户重连，仍待目标验收。没有合适 HDMI 时，当前约束下同会话自动补屏仍未解决。

历史部署/客户端结果仅适用于当时记录版本。本修正的 dummy CI 验证不代表 Intel 显卡、GDM 无屏登录或本版本官方客户端已经完成车端验收。

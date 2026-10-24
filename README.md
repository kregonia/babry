<div align="center">
  <img src="docs/images/babry-mark.svg" width="112" alt="Babry logo">

  # Babry

  **一款为 Linux Wayland 打造的、懂边界的截图工具。**

  悬停吸附 · 单击确认 · 拖动精修 · 原位标注 · 一键复制

  <p>
    <a href="https://github.com/kregonia/babry/actions/workflows/cargo-linux-multiarch.yml"><img src="https://github.com/kregonia/babry/actions/workflows/cargo-linux-multiarch.yml/badge.svg" alt="Build status"></a>
    <a href="https://github.com/kregonia/babry/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-7c5cff.svg" alt="Apache-2.0 license"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/built_with-Rust-dea584.svg" alt="Built with Rust"></a>
    <a href="https://aur.archlinux.org/packages/babry-bin"><img src="https://img.shields.io/aur/version/babry-bin" alt="AUR babry-bin version"></a>
    <a href="https://github.com/kregonia/babry/stargazers"><img src="https://img.shields.io/github/stars/kregonia/babry?style=flat&color=ffd166" alt="GitHub stars"></a>
  </p>

  <p>
    <a href="#快速开始">快速开始</a> ·
    <a href="#为什么是-babry">为什么是 Babry</a> ·
    <a href="#交互方式">交互方式</a> ·
    <a href="#开发者">开发者</a>
  </p>
</div>

<br>

<div align="center">
  <img src="docs/images/selection-editor.png" alt="Babry 编辑器与智能选区预览" width="92%">
</div>

## 为什么是 Babry

传统截图工具通常让你在“手动框选”和“调用桌面环境原生选择器”之间二选一。Babry 走第三条路：**截取整屏后，用本地 CV 结合边缘、色差和布局分析候选区域，在鼠标悬停时给出选区。**

你可以：

- **悬停就吸附**到窗口、面板、卡片或控件；
- **滚轮换层级**，在控件、面板、窗口和全屏之间切换；
- **单击就确认**，不需要反复调整边框；
- **拖动就精修**，任何时候都能切换为像素级手动框选；
- 在同一个界面完成标注、文字、复制、保存和贴图。

> [!NOTE]
> Babry 当前支持 Linux Wayland 下的 niri、Hyprland 和 KDE Plasma 6.8+。原生编辑器使用 SCTK、Layer-Shell 与 wl_shm；智能选区正在迁移到低内存流式检测器。

## 预览

### 智能选区与编辑器

<div align="center">
  <img src="docs/images/selection-editor.png" alt="智能选区、遮罩和标注工具栏" width="96%">
</div>

悬停目标区域时，Babry 会显示候选框、尺寸和四角控制点；进入编辑后，选区外区域自动变暗，视觉焦点留在需要保存的内容上。

### 原位文字输入

<div align="center">
  <img src="docs/images/inline-text.png" alt="原位文字输入预览" width="78%">
</div>

文字工具参考 PixPin 的输入体验：点击画布后直接输入，文字显示在原位置；字号和颜色在工具栏实时调整，输入效果与最终导出保持一致。

### 工具栏细节

<div align="center">
  <img src="docs/images/toolbar-context.png" alt="Babry 工具栏细节" width="96%">
</div>

工具栏提供矩形/圆形/方形、画笔、文字、递增数字标注、马赛克/高斯模糊、橡皮擦、荧光笔、撤销/重做、贴图、保存和复制。形状和效果可从右侧箭头打开菜单切换，左侧点阵用于拖动工具栏。

## 功能一览

| 能力 | 体验 |
| --- | --- |
| **CV 智能选区** | 自适应边缘、颜色边界、背景连通域与长直线分析；质量评分、原图边界精修、滚轮切换层级 |
| **全屏截图** | `grim`（niri/Hyprland）或 KWin 截屏接口（KDE Plasma 6.8+）获取当前 Wayland 桌面，`--screen` 直接进入编辑 |
| **标注工具** | 画笔、矩形/圆形/方形、圈号递增数字标注、橡皮擦和半透明荧光笔 |
| **隐私处理** | 局部马赛克与连续轨迹模糊，可调节模糊度 |
| **原位文字** | 直接在画布输入，多行换行；字号、字体和颜色与导出同步 |
| **工作流** | 实时预览、撤销/重做、保存 PNG、复制到剪贴板、Wayland 贴图 |
| **轻依赖** | Rust + SCTK + wl_shm；不要求 Slint、系统 OpenCV、slurp 或 niri 选区命令 |

## 快速开始

### 1. 安装 Babry

Arch Linux 及其衍生发行版用户可以通过 AUR 安装二进制包 [babry-bin](https://aur.archlinux.org/packages/babry-bin)，无需本地编译 Rust：

```bash
yay -S babry-bin
# 或
paru -S babry-bin
```

如果已配置提供 `babry-bin` 的二进制软件仓库，也可以使用：

```bash
sudo pacman -S babry-bin
```

`pacman` 不直接从 AUR 安装软件；仅使用 AUR 时，请选择上面的 `yay` 或 `paru` 命令。

<details>
<summary>从源码构建</summary>

安装 Rust 工具链及对应发行版的构建依赖后执行：

```bash
git clone https://github.com/kregonia/babry.git
cd babry
cargo build --release --locked
```

生成的二进制文件为 `target/release/babry`。将它放入 `PATH`：

```bash
install -Dm755 target/release/babry ~/.local/bin/babry
```

</details>

### 2. 安装运行依赖

截图在 niri/Hyprland 上需要 `grim`；KDE Plasma 6.8+ 直接调用 KWin 自带的 `org.kde.KWin.ScreenShot2`，无需额外安装。复制图片需要 `wl-clipboard`；可选的 `notify-send` 用于状态通知。AUR 包已声明 Fontconfig、xkbcommon 和 Wayland 系统库依赖，截图和剪贴板工具请按需安装。

Arch Linux：

```bash
# niri / Hyprland
yay -S grim wl-clipboard libnotify

# KDE Plasma 6.8+（无需 grim）
yay -S wl-clipboard libnotify
```

### 3. 开始截图

```bash
babry                  # CV 智能区域截图：悬停吸附，单击确认
babry --smart          # 同上，显式使用智能选区
babry --screen         # 截取全屏并直接进入编辑
babry --pin image.png  # 将已有 PNG 作为屏幕贴图
babry --output DP-1 --screen # 指定显示器截图
babry --list-outputs   # 列出当前会话的显示器信息
babry --help           # 查看完整帮助
```

`--window` 和 `-w` 仍作为兼容别名保留，但现在使用 Babry 自己的 CV 选区，不调用 `niri msg`。

### 图片保存目录

未配置时，图片保存到 `XDG_PICTURES_DIR` 环境变量指定的目录，否则使用 `~/Pictures`；没有 `HOME` 时使用当前目录。文件名为 `babry-年月日-时分秒-毫秒.png`。保存、复制和贴图都会先在此目录生成 PNG。

持久设置可写入 `~/.config/babry/config.toml`；设置了 `XDG_CONFIG_HOME` 时，改用 `$XDG_CONFIG_HOME/babry/config.toml`。示例见 [config.example.toml](docs/config.example.toml)：

```toml
save_dir = "~/Pictures/Screenshots"
```

单次指定或通过环境变量覆盖：

```bash
babry --save-dir ~/Pictures/Work
babry --screen --save-dir "/mnt/data/我的截图"
babry --standalone --output DP-1 --save-dir ./screenshots
BABRY_SAVE_DIR=~/Pictures/Babry babry
```

优先级为 **`--save-dir` → `BABRY_SAVE_DIR` → 配置文件 `save_dir` → `XDG_PICTURES_DIR` → `~/Pictures`**。配置值支持 `~/`、`$HOME/`、`${HOME}/`；配置中的相对路径以配置文件所在目录为基准，命令行和环境变量中的相对路径以本次调用的工作目录为基准。目录只在实际导出时创建；创建或写入失败会报告错误并保留编辑窗口。

后台模式每次截图由调用端重新读取配置并传递保存目录，修改配置无需重启后台，临时 `--save-dir` 不会改变后续截图的默认目录。配置文件不会自动创建或覆盖。

### 常驻后台加速唤起

在图形会话中先启动一次后台：

```bash
babry --daemon
```

该命令在前台运行，可由合成器管理。后台启动后，原有的 `babry`、`babry --screen` 和 `--output NAME` 截图命令会通过当前 Wayland 会话的 Unix socket 唤起它，无需修改截图快捷键。每次请求都会获取新截图；已有截图正在选择或编辑时，重复请求会提示忙碌，不覆盖当前内容。保存、复制、贴图、取消或关闭编辑窗口后，后台释放该次截图、标注历史和 CV 候选，继续等待。Pin 仍运行在独立进程中。

```bash
babry --daemon-status          # 查看当前会话的后台状态
babry --stop-daemon            # 退出后台（同时关闭尚未完成的编辑窗口）
babry --standalone             # 本次跳过后台，截图结束后退出
babry --standalone --screen    # 独立全屏截图，可继续组合 --output
```

后台未运行时，普通截图命令自动按独立模式执行，不强制常驻。后台模式下，命令返回表示截图请求已接收；后续截图失败通过通知和后台 stderr 报告。

niri 可在配置顶层加入：

```kdl
spawn-at-startup "babry" "--daemon"
```

Hyprland 可加入：

```ini
exec-once = babry --daemon
```

常驻后台只监听 Unix socket；截图编辑会话按请求启动并在保存/取消后退出，因此不会在空闲状态初始化 Wayland surface、字体或绘图缓冲区。

## 交互方式

### 选区阶段

1. 移动鼠标：从已缓存的候选中选择边界可靠的区域，微小移动保持当前框。
2. 滚轮向上：扩大到父区域；向下：缩小到鼠标所在的子区域。
3. 单击：确认当前候选，包括滚轮选择的层级，进入编辑阶段。
4. 按住拖动：移动超过 3px 后切换为手动框选；松开后确认区域。
5. 选区边框或角点：在选区阶段也可以直接拖动调整边界，悬停时显示对应缩放光标。
6. `Esc`：取消截图。

CV 分析在每次截图时由后台线程执行一次，悬停不会重新处理整张图像；等待期间可以直接拖动框选。选区评分优先考虑方向边界的连续性，降低文字碎片和不规则纹理的优先级；高分屏候选会在原始截图上精修边界。

### 编辑阶段

- **默认状态**：确认选区或使用 `--screen` 后，标注工具均不选中，拖动选区内部整体平移。
- **工具切换**：点击工具启用对应标注；再次点击同一工具取消选择并恢复选区移动。工具栏没有额外的“编辑”“移动”按钮。
- **调整大小**：四个角点和四条边始终可以拖动调整选区，任何工具状态均有效；悬停显示横向、纵向或对角缩放光标。
- **移动工具栏**：按住最左侧点阵并拖动，只改变工具栏位置，不改变选区或当前工具。
- **形状**：矩形右侧下拉可切换矩形、圆形和方形；圆形/方形保持像素宽高一致。
- **粗细**：画笔等工具下方的滑块和数字输入框共用 1～64px 粗细值。
- **数字标注**：点击选区添加圈 1、圈 2……；撤销和重做同步恢复编号。
- **橡皮擦**：沿笔迹还原原截图像素，擦除也支持撤销/重做。
- **隐私效果**：马赛克右侧下拉可切换高斯模糊。
- **预留功能**：更多入口暂空；折线以及尚未实现的长截图、录屏、OCR、翻译不显示在工具栏。
- **文字**：选择 `字` 工具，在选区内点击即可原位输入。
- **确认文字**：点击画布外、切换工具或按 `Ctrl+Enter`。
- **取消文字**：`Esc`；未输入文字时，先取消当前绘制或选中工具，再次按 `Esc` 关闭截图。
- **字号/颜色**：输入过程中也可以从工具栏实时修改；颜色圆点旁的彩虹按钮可展开默认色盘，选择更多彩色、灰阶和常用预设色。
- **历史**：`Ctrl+Z` 撤销，`Ctrl+Shift+Z` 或 `Ctrl+Y` 重做。
- **导出**：`Ctrl+S` 保存，`Ctrl+C` 复制，工具栏上的图钉用于贴图。

### 贴图阶段

通过工具栏的贴图按钮，Babry 会把当前选区导出后以 Wayland layer-shell Top layer 显示：

- 左键拖动：移动贴图
- 滚轮：缩放贴图
- 右键：关闭贴图
- `Esc`：关闭获得焦点的贴图

## 合成器与多显示器

Babry 会根据 `XDG_CURRENT_DESKTOP` 与合成器环境变量（`NIRI_SOCKET`、`HYPRLAND_INSTANCE_SIGNATURE`、`KDE_FULL_SESSION`）选择 niri、Hyprland 或 KDE Plasma 6.8+，读取每个显示器的名称、逻辑位置、逻辑尺寸和缩放比例。

```bash
babry --list-outputs
babry --output DP-1 --screen
babry --output DP-1 --pin image.png
```

`--output NAME` 可以与截图或贴图命令组合。niri 使用 `niri msg -j outputs`，Hyprland 使用 `hyprctl -j monitors`；Hyprland 的物理尺寸会按缩放比例和旋转转换为逻辑尺寸。KDE 上输出信息通过 Wayland `xdg_output` 读取，分数缩放由当前 mode 的物理尺寸除以逻辑尺寸推导（KWin 的 `wl_output.scale` 是向上取整的整数），焦点显示器通过 `org.kde.KWin.activeOutputName()` 查询。默认启动会先确定焦点显示器，再只截取该屏幕：niri/Hyprland 调用 `grim -o NAME`，KDE 调用 KWin 的 `org.kde.KWin.ScreenShot2` D-Bus 接口（返回原始像素，不经过 XDG Portal；Plasma 6.8 起无需额外桌面文件声明）。无法获取焦点时使用第一个启用的显示器；niri/Hyprland 的 IPC 查询失败会报告错误，KDE 的焦点查询失败会记录警告并回退到第一个输出。指定不存在的显示器会报错。

当前截图按指定显示器单独捕获，贴图初始尺寸也按目标显示器的逻辑尺寸和缩放计算。跨显示器拖动、把同一张贴图动态迁移到另一块输出，以及不同输出间的实时 layer-shell 重绑定仍在完善中。

### KDE Plasma 6.8+

- 截图走 KWin 自带的 `org.kde.KWin.ScreenShot2`（Spectacle 同款），无需安装 `grim`。
- 编辑器窗口在 KDE 上全屏覆盖桌面（含面板）；niri/Hyprland 的全屏行为保持不变，继续使用各自窗口规则。
- Plasma 6.8 之前的 ScreenShot2 需要桌面文件声明 `X-KDE-DBUS-Restricted-Interfaces`，Babry 暂不支持这些版本。
- 窗口创建瞬间任务栏可能闪现一帧条目，Alt+Tab 中也可能可见；这是 KWin 在窗口 map 与全屏生效之间的行为，不影响截图。
- KWin 在锁屏/熄屏期间返回黑帧，属安全行为，与 XDG Portal 一致。

## niri 配置

Babry 不依赖 niri 来完成选区，但可以用 niri 的窗口规则让编辑器以全屏方式显示。将以下内容放入你的规则文件：

```kdl
window-rule {
    match app-id=r#"^babry$"#
    open-fullscreen true
    open-focused true
}
```

修改后重新加载：

```bash
niri msg action load-config-file
```

推荐快捷键：

```kdl
binds {
    Mod+Shift+S { spawn "babry"; }
    Mod+Shift+F { spawn "babry" "--screen"; }
}
```

## 字体配置

文字标注会优先使用 `BABRY_FONT` 指定的字体，并尝试系统中文字体。适合中文环境的示例：

```bash
export BABRY_FONT=/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc
export BABRY_FONT_INDEX=0
babry
```

TrueType Collection（`.ttc`）可以通过 `BABRY_FONT_INDEX` 选择字体索引。输入阶段和导出阶段使用同一字体来源，减少“输入时一种字体、保存后另一种字体”的偏差。

## 技术路线

```text
Wayland 桌面
    │
    ▼
  grim（niri/Hyprland）或 KWin ScreenShot2（KDE Plasma 6.8+）
    │
    ▼
  内存原图（PPM 管道 / 原始像素）
    │
    ▼
  原生流式 CV 检测器
    │
    ▼
  SCTK Layer-Shell 选区交互层
    │
    ▼
  wl_shm CPU 标注编辑器
    │      │      │
    ▼      ▼      ▼
 PNG 保存  剪贴板  layer-shell 贴图
```

### 项目结构

```text
src/
├── cli.rs              # 命令解析与帮助文本
├── compositor.rs       # 合成器检测、输出元数据与窗口锚点
├── config.rs           # 保存目录配置、路径展开与导出文件名
├── daemon.rs           # 会话 Unix socket、实例锁和后台子进程
├── font.rs             # 按需 mmap 字体与 ttf-parser 轮廓
├── kde.rs              # 按需加载 sd-bus 的 KWin ScreenShot2
├── marks.rs            # CPU 像素标注命令与基础光栅化
├── native.rs           # memfd/wl_shm 共享内存基础设施
├── native_pin.rs       # SCTK layer-shell Pin
├── pixels.rs           # RGBA 像素、PPM/PNG 编码与裁剪
├── screencopy.rs       # zwlr_screencopy SHM 捕获
├── core/
│   ├── geometry.rs     # 像素几何与归一化坐标
│   └── history.rs      # 撤销/重做历史
├── lib.rs              # 库模块导出
└── main.rs             # 原生 Layer-Shell 编辑器与工作流
```

## 开发者

常用验证命令：

```bash
cargo fmt --all
cargo check --locked
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
```

当前测试覆盖 CLI、几何、历史、编辑渲染、字体文字导出、局部画布、撤销重做、贴图尺寸与拖动位移，以及弱边界、等亮度色块、圆角、密集文字和 4K 小控件的 CV 回归；合成器后端覆盖 niri/Hyprland JSON 解析、KDE raw 像素解包、分数缩放推导和会话检测。界面事件测试验证滚轮层级切换、单击确认、手动拖框和局部图像生命周期。

### 启动性能定位

niri/Hyprland 使用 `zwlr_screencopy_manager_v1` 直接写入 wl_shm；KDE 使用按需加载的 ScreenShot2 sd-bus 兼容层。编辑器直接绘制到 SCTK wl_shm 帧，最终保存使用 PNG 编码器。当前原生迁移仍在进行，尚未宣称捕获源与显示帧完全零拷贝。

设置 `BABRY_TRACE_STARTUP=1` 后，stderr 会输出 JSON 阶段耗时：

```bash
BABRY_TRACE_STARTUP=1 babry --standalone --screen
BABRY_TRACE_STARTUP=1 babry --daemon
# 在另一终端运行 babry 或 babry --screen
```

原有 Slint 渲染埋点已随原生迁移移除；后续将使用 Wayland frame callback 和实际提交时间重新定义。不要把旧版本测量当作原生重构后的结果。

### 编辑性能与基准

原生编辑器使用 wl_shm 显示帧、显式 buffer release 生命周期和矢量标注命令；当前阶段仍在补齐智能选区、文字输入、撤销重做和多帧脏矩形重绘。不要把本阶段的基础实现当作最终性能结果。

运行可重复的编辑热点基准：

```bash
cargo test --release --locked --all-targets
```

基准使用 4K 原图、960×540 选区，测量 120 次短自由笔预览和 20 次提交，输出耗时与 CPU 像素缓冲区大小。它不包含完整 UI 帧率、GPU 纹理或整个进程 RSS；大范围笔迹与全屏选区仍需要更多计算和内存。

当前原生重构的基础检查：

```bash
cargo test --locked --all-targets
```

集成测试同时检查缩放复用缓冲区，以及重复指针事件、松手同帧的末次移动、四边四角缩放和边界反向拖动。niri 的隐式抓取坐标与 KWin 的表面局部坐标分别处理，避免表面移动回发事件造成贴图持续漂移；位置和尺寸未变化时不会重复提交。

### 检查 CV 选区

对已保存的截图运行离线诊断，无需启动 Wayland 截图窗口：

```bash
babry --debug-selection screenshot.png /tmp/babry-cv-debug
```

输出目录包含：

- `candidates.png`：所有保留候选及其编号；绿色表示质量评分至少 0.85，橙色表示较弱的候选。
- `candidates.json`：对应编号的矩形、检测来源、四侧边界支持度、内部边缘密度、质量评分、排序值和分析耗时。

评分用于启发式排序，不是识别正确率；排序值越小越优先。原图、鼠标位置和期望选区可以一起用于复现误判。

## 当前边界

- 编辑器在 niri 下依赖 `open-fullscreen` 窗口规则；未配置时会按普通窗口显示。KDE Plasma 6.8+ 会自动全屏覆盖桌面。
- CV 识别不到明确边界时会回退到全屏，仍可拖动鼠标精确框选。
- 跨显示器拖动 Pin、OCR、贴图持久化和滚动长截图尚未实现。
- Pin 支持 `wp_viewporter` 时，拖动缩放直接由合成器完成；不支持时使用 CPU 重采样回退。

## 参与贡献

欢迎提交 Issue、改进建议和 Pull Request。特别欢迎：

- 不同 Wayland 合成器上的兼容性反馈；
- 中英文、日韩文字体的渲染样例；
- CV 选区误判截图与可复现图片；
- 新标注工具和更顺手的快捷键设计。

如果 Babry 对你的工作流有帮助，欢迎点一个 ⭐，它会帮助项目被更多 Wayland 用户发现。

## License

[Apache License 2.0](LICENSE)

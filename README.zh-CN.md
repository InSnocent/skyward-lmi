![Skyward Recovery](assets/hero.jpg)

# Skyward Recovery · 红米 K30 Pro

[English](README.md) | 简体中文

Skyward Recovery 的红米 K30 Pro（`lmi`）非官方移植，当前处于预览阶段。Skyward 是用 Rust 编写的 Android Recovery。下列功能描述当前源码；此前发布或已安装的镜像可能尚未包含最新改动。 [上游 Skyward](https://github.com/kirklin/skyward)

lmi v1.2.3 基于上游 Skyward v0.3.1，已在 Redmi K30 Pro 上验证完整格式化 Data、安装官方 LineageOS 23.2 和首次开机。

## 截图

图中依次为首页、壁纸设置和深色毛玻璃卡片。界面由电脑端渲染，使用示例壁纸。

<p align="center">
  <img src="assets/lmi/home.png" width="30%" alt="红米 K30 Pro 的 Skyward 首页">
  <img src="assets/lmi/wallpaper.png" width="30%" alt="壁纸导入和不透明度设置">
  <img src="assets/lmi/frosted-dark.png" width="30%" alt="开启毛玻璃卡片的深色外观设置">
</p>

## 功能

### 解密

在「解密」中输入当前 Android 锁屏 PIN，解锁内部存储。PIN 解密已在下表所列 ROM 构建上实测。

### 通用 ZIP 安装

支持通过 ADB sideload、解密后的内部存储或 OTG U 盘选择 `lmi` 完整 Block OTA，以及包含 `update-binary` 的传统脚本 ZIP。选择后先验签并显示文件的 SHA-256；完整 ROM 还会显示机型及声明的目标分区。未通过可信签名验证时，可在安装页仅对这一份 ZIP 确认继续，随后仍需将滑条拖到底。返回可取消。

「ADB sideload 只读预检」检查签名和包结构，不运行包内程序。公开测试密钥签署的包会明确标注「来源未验证」，安装这类包也需要单次放行。较早的固定包版本曾实测通过 ADB 重装同版 Infinity-X。v1.2.3 已实测通过通用 ADB 入口刷入 LineageOS。 [ZIP 兼容范围与测试](docs/lmi.zh-CN.md#通用-zip-安装)

### 本地刷入 boot 和 recovery 镜像

解密后打开「安装 → 从内部存储安装」，选择 `.img`。Skyward 根据 ramdisk 判断目标是 `boot` 还是 `recovery`，并显示所选文件的完整 SHA-256。请与可信来源为同一机型和系统构建提供的摘要核对，确认后滑动刷入。写入结束后，Skyward 会回读整个目标分区，并核对完整镜像的 SHA-256。 [镜像格式与操作说明](docs/lmi.zh-CN.md#本地-img-刷入)

### 备份和恢复

备份 `/data` 中受支持的内容，并在备份结束后回读校验。可在「恢复」中查看备份，并选择「校验备份」单独检查清单、分卷长度和内容摘要；支持中途停止，不执行恢复。已在支持的 lmi 构建上验证停止及重新完整校验。勾选「共享文件」可备份机主的照片和下载。恢复时会写入内部存储的 Skyward/restored 下的新目录。恢复流程已用测试应用验证，整份 `/data` 写回仍待实机验证。 [备份说明](docs/lmi.zh-CN.md#数据备份)

「恢复」可识别通过 OTG 接入的 FAT32 或 exFAT U 盘，未解密也能查看和校验备份。开发候选版加入了备份 data 到 U 盘及从 U 盘恢复；两项都需要先完成解密。exFAT 的只读挂载、校验、拔插、备份取消和合成数据备份恢复均已实测。FAT32 和整份 data 恢复仍待实机验证。

### 壁纸和外观

解密后可从内部存储选择 JPG 或 PNG。在裁剪页拖动图片，按屏幕比例取景，也可以用滑块缩放。框外显示压暗的原图，便于对齐主体。点「应用壁纸」后保存。壁纸不透明度可在 5%–100% 之间调节。开启「设置 → 外观 → 毛玻璃卡片」后，主要页面在浅色、深色模式下都能显示壁纸和毛玻璃。再次进入 Recovery 并解密时，会加载保存的壁纸和开关状态。

### 其他功能

- 基础设置保存到 `/metadata/skyward/settings.json`，在解密前即可恢复和保存。可在「设置 → 语言」中直接切换语言，无需解密。
- 支持触摸、亮度调节和振动反馈。
- 可只读查看系统逻辑分区。
- 可从 Recovery 菜单重启到 Android。
- 文件管理可浏览解密后的内部存储，重命名和复制已用测试文件验证；终端、USB 文件传输（MTP）、日志导出和截图已在真机进行基本验证。

## 当前限制

部分上游菜单仍会显示。点到尚未接入的操作时，界面会提示「此操作在界面测试中停用」。

- **高级工具**。文件管理的其他写入操作及大量文件处理仍需进一步验证；包含非 UTF-8 字节的文件名可显示，但暂不能通过图形界面操作；MTP 不显示非 UTF-8 名称或超过协议长度上限的名称。文件管理写入、文本编辑及终端任务期间，MTP 会暂时撤出内部存储。终端主动脱离会话的后台进程无法完全追踪，备份、恢复和安装前需自行确认它们已结束。SELinux 上下文修复尚未适配。
- **维护和重启**。「高级清除」可分别勾选应用私有数据、内部存储、应用缓存、系统缓存和 Dalvik / ART 缓存。识别到独立 cache 分区时也可单选。「格式化 data」有独立确认页，重启后由系统重建加密数据分区。已在 Redmi K30 Pro 上实测完整格式化及 LineageOS 首次启动后的文件级加密。挂载页只读显示系统逻辑分区。重启菜单可返回 Android。
- **安装**。通用 ADB sideload 已实测安装官方 LineageOS 23.2 并首次启动。内部存储或 U 盘写入正式 ROM，以及各类实际脚本包的兼容性仍待验证。A/B payload、增量 OTA 或双系统包不受支持。IMG 仅支持 `lmi` 的独立 `boot` 和 `recovery` 分区，要求 Android v2 引导头、4096 字节页、包含内核，文件最大 128 MiB。通用镜像无法自动证明机型匹配，安装器也不验证 IMG 的签名或来源。其他 IMG 分区、从 U 盘安装 IMG 和在线更新仍待适配。

## 支持的机型

| 机型 | 代号 | 已验证的 ROM | 构建号 |
| --- | --- | --- | --- |
| 红米 K30 Pro | `lmi` | Infinity-X 3.12 非官方版（Android 16） | `1793266721` |

实测范围为表中的机型和 ROM 构建。 [对应的 Infinity-X 构建](https://github.com/InSnocent/infinityx-lmi)

## 安装 Skyward

准备已解锁的 Bootloader、Android SDK Platform-Tools，以及与当前系统配套的原 Infinity-X Recovery 镜像。使用发布时提供或构建时生成的 SHA-256 校验 Skyward 镜像。执行下方命令时，把 `skyward-lmi.img` 换成实际镜像文件名。 [已记录的 SHA-256](docs/lmi.zh-CN.md#sha-256)

### 临时启动

进入 Bootloader 的 FASTBOOT，核对设备代号。

```sh
fastboot getvar product
```

确认手机是红米 K30 Pro 且返回 `lmi` 后，临时启动镜像。

```sh
fastboot boot skyward-lmi.img
```

确认能进入 Skyward、用原 PIN 解密，并从「重启」菜单返回 Android。

### 常驻安装与回退

再次进入 Bootloader 的 FASTBOOT，核对设备代号后刷入已测试的镜像。

```sh
fastboot flash recovery skyward-lmi.img
fastboot reboot
```

Android 启动后关机，同时按住音量上和电源键进入 Skyward。需要回退时，把当前 ROM 配套的原 Infinity-X Recovery 刷回同一分区。 [回退步骤](docs/lmi.zh-CN.md#永久安装与回退)

## 构建

使用 Linux 或 WSL、Rust 的 `aarch64-linux-android` 目标和 Android NDK r29。基础 Recovery 和其他打包输入取自对应的 Infinity-X 构建。 [构建命令与输入文件](docs/lmi.zh-CN.md#从源码构建)

## 许可证

[GPL-3.0-or-later](LICENSE)

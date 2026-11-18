# Redmi K30 Pro（lmi）的 Skyward Recovery 移植

本移植基于 [Skyward v0.2.0 的上游源码](https://github.com/kirklin/skyward/tree/ff1668fdb72a46f22152a6540f49728c42081aba)，在已解锁 Bootloader 的 Redmi K30 Pro（`lmi`）上测试。真机运行的是 [Infinity-X 3.12 非官方版](https://github.com/InSnocent/infinityx-lmi)，系统为 Android 16，构建号为 `1793266721`。

## 实机验证

| 项目 | 结果 |
| --- | --- |
| 临时启动与界面 | 通过 `fastboot boot` 进入中文界面，显示、触摸、返回和重启到系统可用 |
| 亮度与振动 | 亮度滑块可改变屏幕亮度，AW8697 触感反馈在高低档有区别 |
| 系统识别与挂载 | 显示 Infinity-X 3.12 和 Android 16，可只读挂载 `system`、`vendor`、`product`、`odm` 和 `system_ext` |
| ADB sideload 验签 | 有效签名的测试 ZIP 执行了测试脚本，改动后的 ZIP 在确认执行前被拒绝 |
| 同版 ROM 重装 | 指定完整 ZIP 通过只读预检和实际写入，重启后系统可解锁，共享存储可访问，抽查的应用资料仍在 |
| SukiSU 恢复 | 同版 ROM 覆盖 `boot` 后，重新写入配套的 SukiSU 4.3.0 boot。重启后 `su -c id` 返回 `uid=0`，`boot` 哈希与镜像一致 |
| 永久安装与回退 | 将候选版刷入 `recovery` 后，关机可用音量上＋电源进入 Skyward，并可从菜单回到系统。刷回原 Recovery 后成功返回系统，之后重新装回 Skyward。两次切换均核对了实际分区哈希及 SukiSU Root |
| 原 PIN 解密 | 临时数据备份版可挂载 F2FS，用原 PIN 解锁应用私有数据 |
| 完整数据备份 | 用「备份」页面为 `/data` 创建 LZ4 压缩备份并通过回读校验，「恢复」页面可以识别备份 |
| 测试应用恢复 | 使用与完整备份相同的归档和恢复代码。删除测试应用的私有文件、SharedPreferences 和 SQLite 数据库后，从备份写回 F2FS。三份文件的 SHA-256、权限、所有者和 SELinux 标签均与原件一致，Android 重启后由应用自身读取通过 |
| 合版 Recovery | 原 PIN 解密后可识别已有备份。以上述 ROM 通过只读 sideload，报告列出 5 个逻辑分区和 4 个独立分区。刷入合版后，音量上＋电源冷启动、查看备份和返回 Infinity-X 均通过，SukiSU Root 可用 |
| 壁纸与毛玻璃版 | 各测试版分别验证了壁纸导入、两种主题、滚动、重启后解密恢复，以及状态栏和 5% 下限。最新版刷入后从 Android 回读的 `recovery` 分区哈希与镜像一致，Infinity-X 和 SukiSU Root 可用 |
| 临时功能版 | 临时启动后验证了安装、备份和高级等页面的壁纸与毛玻璃；文件管理浏览、终端的 `pwd`、`ls`、`cd`、MTP 文件传输、日志导出与截图可用。仅测试过指定样本的文件日期和损坏 ZIP 的失败返回 |
| 内部存储预检 | 解密后从内部存储选择匹配的 Infinity-X ZIP，通过签名、SHA-256 和分区清单检查，在确认写入前返回；实际本地安装写入尚未验证 |
| 内部存储模拟安装 | 使用独立测试证书和无真实分区目标的模拟 ZIP，确认后释放逻辑分区映射，测试脚本仍能读取内部存储标记和安装包；正式 ROM 的本地写入尚未验证 |
| 虚拟目录与重命名光标 | `/config` 的 configfs 原始时间戳为 1，界面显示时间未知且详情保留原始值；重命名时点击中间位置插入字符通过真机验证 |
| 测试文件操作 | 使用单独测试文件完成重命名和复制，通过 MTP 回读后，两份文件的 SHA-256 均与原文件一致；其他文件写入操作仍需验证 |
| 特殊文件名与 MTP 互斥 | 非 UTF-8 测试名称被图形界面拦截，同显示名称的合法文件可打开。终端运行时 MTP 撤出存储，退出后恢复；MTP 开启时重命名测试文件，恢复后回读的 SHA-256 与原件一致 |
| 恢复页解密提示版 | 未解密时进入恢复页，点击提示完成解密；点「完成」后无需重新进入，即可看到已有备份。只查看，未执行整份恢复。刷入后音量上＋电源冷启动、返回 Android、Recovery 分区哈希和 SukiSU Root 核对通过 |
| 解密前基础设置 | 旧版六项设置已完整迁移到 `/metadata/skyward/settings.json`。再次启动时，语言、主题、亮度、振动、屏幕关闭时间和时区在解密前均已恢复。首页可直接打开语言选择，未解密切换 English 和浅色后，配置文件回读核对通过 |
| 独立校验备份 | 临时启动后，原备份可以开始校验、显示进度并停止；原备份仍可打开，重新完整校验通过。只校验，未执行恢复 |

实机验证范围限于上述 Redmi K30 Pro 和 Infinity-X 构建。合版验证了只读预检以及解密服务和挂载的正常释放。实际 ROM 写入由此前的安装版验证。未接通的菜单和完整恢复的限制见首页。 [当前限制](../README.zh-CN.md#当前限制)

## 临时启动与同版重装

以下命令使用 `skyward-lmi.img` 作为构建产物的文件名。使用已有候选镜像时，请填入实际文件名，并核对本页的 SHA-256。

准备已解锁的 `lmi`、USB 数据线和 Android SDK Platform-Tools。重装前备份重要资料，并保留与当前系统匹配的 Infinity-X Recovery 和 ROM 自带的 `boot.img`。

1. 让手机进入 Bootloader 的 FASTBOOT。用 `fastboot getvar product` 核对返回的代号是 `lmi`。
2. 核对 Skyward 镜像的 SHA-256 后临时启动。

   ```sh
   fastboot boot skyward-lmi.img
   ```

3. 进入「安装」→「ADB sideload 只读预检」。在电脑发送同版完整 ZIP，核对签名与分区清单。需要重装时，返回「安装」选择「ADB sideload 安装系统」，再发送同一个 ZIP。

   ```sh
   adb sideload Project_Infinity-X-3.12-lmi-29.10.2026-GAPPS-UNOFFICIAL.zip
   ```

4. 两个入口均核对原 Infinity-X Recovery 的 OTA 签名和固定的 ZIP SHA-256。只读预检显示分区清单后结束。安装系统会继续显示滑动确认，开始后请保持 USB 连接，直到显示「安装成功」，再选「重启到系统」。
5. 用原 PIN 解锁，并核对共享存储和应用资料。如需恢复 Root，使用与这个构建匹配的 SukiSU boot，先临时启动验证，再刷入 `boot` 并重启复测。 [Infinity-X lmi 项目](https://github.com/InSnocent/infinityx-lmi)

重装会重建 `super` 中的逻辑分区，并覆盖 `boot`、`dtbo`、`vbmeta` 和 `vbmeta_system`。ADB sideload 确认后，Skyward 停止解密服务、关闭数据分区并释放自己的逻辑分区映射，再将写入交给 ROM 自带的 `update-binary`。流程没有格式化步骤。实际同版重装后，SukiSU 失效，原 Recovery 再次出现；如需继续使用，应重新验证并刷入配套的 Root 镜像和 Skyward。

内部存储安装需要保持数据分区打开以读取 ZIP；ROM 的脚本会清除部分系统缓存。此路径已验证到确认页面，实际 ROM 写入尚未验证。使用 `fastboot boot` 临时启动后，重启进 Recovery 会进入手机已安装的 Recovery。若安装结果页报错，保留画面和 USB 连接以便排查。 [AOSP 的非 A/B 动态分区 OTA 文档](https://source.android.com/docs/core/ota/dynamic_partitions/nonab)

## 数据备份

进入合版 Skyward 后，先在「解密」中输入当前 Android 的原 PIN，再打开「备份」，选择 `data` 分区、压缩和完成后校验。备份完成后可在「恢复」查看备份时间、大小和来源。

备份保存在内部存储的 `Skyward/backups/`，每份由 `backup.toml` 和连续编号的分卷组成，复制时请带齐整个目录。文件备份涵盖应用及其数据、账户数据库和系统设置。共享存储（照片、下载等）、存储密钥、相关锁屏文件和指纹等需要按各自的方式保留。恢复 `/data` 会覆盖备份范围内的数据，并要求仍使用创建这份备份时的加密数据分区。恢复出厂设置后，Skyward 会拒绝写入此前的 `data` 备份。

### 独立校验备份

在「恢复」中打开一份备份，选择「校验备份」。校验会读取备份清单和全部编号分卷，核对文件长度、解压后的长度和 SHA-256。页面显示进度并支持停止；停止不会删除原备份。校验期间暂时撤出 MTP 存储，结束后恢复原文件传输设置。

校验通过仅说明内容与备份清单一致，不能代替恢复前的设备和加密密钥检查。清单与内容可以同时被改动，因此该检查也不用于认证来源。电脑端已覆盖普通、压缩、分卷、缺失、损坏、取消及压缩尾部残留等测试。在本页所列 lmi 与 Infinity-X 构建上，已临时启动并验证入口、进度、停止，以及同一备份重新完整校验通过；未执行恢复。

## 永久安装与回退

`lmi` 有独立的 `recovery` 分区。在 Bootloader 的 FASTBOOT 中用 `fastboot getvar product` 确认是 `lmi`，按下表核对镜像哈希后执行。

```sh
fastboot flash recovery skyward-lmi.img
fastboot reboot
```

系统启动后关机，同时按住音量上和电源键进入 Skyward。用「安装」核对两个 sideload 入口，再通过「重启」返回系统。 [LineageOS 的 lmi 按键说明](https://lineageos.github.io/lineage_wiki/devices/lmi/install/variant1/)

需要回退时，重新进入 Bootloader 的 FASTBOOT。可用下表中的 `lmi-skyward-block-ota-install-remapped.img` 恢复旧版 Skyward，也可使用与当前系统配套的原 Infinity-X Recovery。

```sh
fastboot flash recovery recovery.img
fastboot reboot recovery
```

进入 Infinity-X Recovery 后可用「Reboot system now」返回系统。

## 从源码构建

在 Linux 或 WSL 中打开包含 `lmi` 改动的源码树。若从补丁复现，将补丁应用于上游提交 `ff1668fdb72a46f22152a6540f49728c42081aba`。实测工具链为 Rust 1.100.0（含 `aarch64-linux-android` 目标）和 Android NDK r29。先将 `ANDROID_NDK_HOME` 指向已解压的 NDK 目录。示例使用 Linux x86_64 主机。 [上游构建说明](https://github.com/kirklin/skyward/blob/ff1668fdb72a46f22152a6540f49728c42081aba/README.zh-CN.md#构建)

```sh
: "${ANDROID_NDK_HOME:?请先设置 Android NDK r29 的目录}"
ndk_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ndk_bin/aarch64-linux-android30-clang"
export CC_aarch64_linux_android="$ndk_bin/aarch64-linux-android30-clang"
export CXX_aarch64_linux_android="$ndk_bin/aarch64-linux-android30-clang++"
export AR_aarch64_linux_android="$ndk_bin/llvm-ar"

cargo test --workspace --locked
cargo build --locked --release -p skyward-pack
cargo build --locked --target aarch64-linux-android --release -p skyward
```

在仓库根目录准备 `inputs/recovery.img`，即同版配套的 Infinity-X Recovery 镜像，并准备从同版 ROM 还原出的 `inputs/vendor.img`。ROM 内的 `vendor.new.dat.br` 需要先用 Brotli 解压，再用 `vendor.transfer.list` 和解压得到的 `vendor.new.dat` 还原为 EROFS 镜像。若已准备 `inputs/vendor.img`，可直接提取固件。从同一 ROM 提取 Runtime 和 ICU APEX 文件到 `inputs/lmi-apex`，保留 `com.android.runtime/` 与 `com.android.i18n/` 的内部路径。所需文件由 `skyward-pack` 的 `add_lmi_apex` 列出。 [sdat2img](https://github.com/xpirt/sdat2img)

```sh
mkdir -p inputs/aw8697
if [ ! -f inputs/vendor.img ]; then
  # 先将完整 ROM 和 sdat2img.py 放在当前目录。
  unzip -j Project_Infinity-X-3.12-lmi-29.10.2026-GAPPS-UNOFFICIAL.zip \
    vendor.new.dat.br vendor.transfer.list -d inputs
  brotli -d -o inputs/vendor.new.dat inputs/vendor.new.dat.br
  python3 sdat2img.py inputs/vendor.transfer.list inputs/vendor.new.dat inputs/vendor.img
fi

for name in aw8697_haptic.bin aw8697_rtp_1.bin; do
  dump.erofs --cat --path="/firmware/$name" inputs/vendor.img > "inputs/aw8697/$name"
done

target/release/skyward-pack preview-ui \
  --read-only --block-ota-preflight \
  --data-readonly --lmi-data-backup --lmi-apex-files inputs/lmi-apex \
  --block-ota-install 9cf66b7fa39e1f5afd2b22dfdb13739bbba6140c9456ab659e1df2434933843e \
  --base inputs/recovery.img \
  --binary target/aarch64-linux-android/release/skyward \
  --config devices/lmi.toml \
  --font resources/fonts/MiSansVF.ttf \
  --mono resources/fonts/KirkMono-Regular.ttf \
  --mono-license resources/fonts/KirkMono-OFL.txt \
  --aw8697-firmware inputs/aw8697 \
  --out skyward-lmi.img
sha256sum skyward-lmi.img
```

## 合版镜像核对

已核对 Android v2 引导头、AVB 哈希描述符、内核、设备树、原 Recovery 的 OTA 证书及固定的 ROM 哈希。早期合版验证了按键冷启动和回退。壁纸与毛玻璃版使用同一份镜像进行临时测试和常驻安装，刷入后从 Android 核对了 recovery 分区和 SukiSU boot 的哈希。

## SHA-256

| 文件 | 实机测试或构建输入的 SHA-256 |
| --- | --- |
| `lmi-skyward-backup-verify-test-v1-20261118.img`（独立备份校验版） | `816e4fc083b8588322fed39d731c8da46f3e66dc95940d48b03a3e024e4be850` |
| `lmi-skyward-settings-preunlock-test-v4-20261118.img`（解密前基础设置版） | `522d5f1251a4fef00e29d811c7263c96474bb906a7ee11b734b10b6c9074b365` |
| `lmi-skyward-restore-refresh-test-v9.img`（恢复页解密提示版） | `b09840c4f0585c2c021b6ca6d9ce3dc0bd93da527ffb1e5893eb9c95bce5c5e1` |
| `lmi-skyward-features-global-test-v2.img`（临时功能版） | `f890cc4e12c3a28519b33d4dbfef139bbbf312dcc4e2edc8fa3363b0cc881d33` |
| `lmi-skyward-security-test-v6.img`（临时文件与 MTP 测试版） | `b76bc2e35619fb3af88703fd4ea736ebf4abebba87f5701b0180df7d856f948f` |
| `lmi-skyward-local-ota-test.img`（临时本地安装预检版） | `1242fe80965d14537a71af3a91cebae9fa58050d04111000976174fb40fc85c0` |
| `lmi-skyward-frosted-wallpaper-tuned.img`（壁纸与毛玻璃版） | `128b7d7cb3666b3834b9327867cebb6c3f20cec8b4aeb9cb5e5d8961a315299f` |
| `lmi-skyward-merged-candidate.img`（早期合版） | `8e196a8806aa2161ce44c7ffd01d84206b9c58c00de5f46fa7322edd3ee5c477` |
| `lmi-skyward-data-backup-preview-v3.img`（临时数据备份版） | `1a016e734a190da8a5b09e008ba80b41ab559b18e7c9c0e9624cb67ba66cdd20` |
| `lmi-skyward-block-ota-install-remapped.img`（旧版回退） | `e90c22c0ee6998b09f7305e8242fd8d7668a9cd10695b5f29ffa06d7168e0933` |
| `lmi-skyward-block-ota-install.img` | `7c8c9cb450e5b70ccf35b6d69f0a2cb31a654f407790d9597e1bc1fc16d75e0c` |
| `Project_Infinity-X-3.12-lmi-29.10.2026-GAPPS-UNOFFICIAL.zip` | `9cf66b7fa39e1f5afd2b22dfdb13739bbba6140c9456ab659e1df2434933843e` |
| `recovery.img` | `35935b135376b51a6dc1eb673d0329ee84c81672cb24add4ef1192fa59f1e331` |
| `boot.img`（ROM 自带） | `21283479f70c40ee77b7b74b953d85072b78ab52e7def464d918aa0356b22cb7` |
| `boot-lmi-infinityx-sukisu-v4.3.0.img` | `898749b73f7daf4c37dcdc98e0cfa8671caa8b4d99c4abc9f15c2cf807aa39c3` |
| `aw8697_haptic.bin` | `c85799567cb7e6842a9968e97f2438a5b0797f260d1dc64d4becd765c088cafd` |
| `aw8697_rtp_1.bin` | `9a413b131ecf0ccfb02a837ddb33766d8604cc00da7937b79bd363b53c8d7d86` |

镜像哈希对应指定的实机测试文件或发布候选文件。自行构建的镜像可能因工具链和环境而有不同哈希。配套 ROM、Recovery 和振动固件按上述输入校验。移植源码遵循 [Skyward 的 GPL-3.0-or-later 许可](https://github.com/kirklin/skyward/blob/main/LICENSE)。

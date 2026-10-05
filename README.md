# rwebman

用一个 Rust 可执行文件，提供需要密码解锁的本地文件搜索与 Web Terminal。前端使用原生 HTML/CSS/JS 与 xterm.js，所有资源通过 `include_bytes!` 编译进程序。运行时不需要 Node.js、前端目录、CDN 或单独安装 SQLite。

## 启动

```powershell
cargo build --release --locked
.\target\release\rwebman.exe
```

首次启动会在**本机命令行**要求设置并确认密码（至少 12 个字符），随后浏览器打开 **http://127.0.0.1:8080**。不会通过公开网页设置初始密码，也没有默认密码。macOS / Linux 使用 `./target/release/rwebman`。

默认启动后台索引，每 60 分钟更新。扫描期间仍可搜索上一次完整索引；首次扫描完成前搜索为空。网页提供文件类型筛选、完整路径匹配、复制路径、分页、扫描进度、取消扫描、索引目录与排除项设置。

只索引文件 / 文件夹 / 符号链接的**名称、路径、大小和修改时间**，不索引文件内容。搜索忽略大小写，输入按字面子串匹配，`%`、`_`、引号不是查询语法。

## 常见用法

```powershell
# 指定端口和多个索引目录；目录设置会持久保存
rwebman.exe --bind 127.0.0.1:8090 --root D:\work --root C:\Users

# 数据与程序放在同一目录附近（可携带部署）
rwebman.exe --data-dir .\data

# 禁用启动和定时扫描，仍可在网页手动更新
rwebman.exe --no-auto-index

# 启动时扫描，此后不定时刷新
rwebman.exe --index-interval 0

# 指定 Shell 程序路径，不传入命令字符串
rwebman.exe --shell C:\Windows\System32\cmd.exe

# 停止本程序后修改密码；如使用自定义数据目录，应同时指定 --data-dir
rwebman.exe --set-password
```

自动化初始化或重设密码时，可以从进程环境变量 `RWEBMAN_PASSWORD` 读取密码。仅在没有配置或指定 `--set-password` 时使用；程序在启动工作线程前移除该变量，Shell 不会继承它。不要把密码写在命令行参数、版本库或共享脚本中。

同一数据目录只允许一个实例运行。程序不会强制结束已有实例；使用其他数据目录和端口可以运行第二个实例。

## 默认索引范围

| 系统 | 自动选择 |
| --- | --- |
| Windows | 所有固定磁盘（`GetLogicalDrives` / `GetDriveTypeW`）；网络盘和可移动盘需要手动添加 |
| macOS | `/Users`、`/Volumes`、`/Applications`、`/opt`、`/usr/local` 以及当前用户主目录中实际存在的目录 |
| Linux / 其他 Unix | `/home`、`/root`、`/mnt`、`/media`、`/srv`、`/opt`、`/usr/local` 以及当前用户主目录中实际存在的目录 |

默认跳过 `.git`、`node_modules`、Windows 系统目录 / 回收站，以及 Unix 虚拟文件系统和部分缓存。完整列表可在「索引设置」中查看和编辑；可以添加 `/` 扫描其他 Unix 位置。存在的挂载点若位于所选根目录下面，也会被遍历。

排除项每行一项：不含分隔符时匹配任意位置的同名文件或目录；绝对路径匹配该路径及其子目录。**不支持 glob 通配符**。程序自己的数据目录始终排除。索引根目录会规范化、去重并移除被其他根目录包含的子目录。目录符号链接不递归跟随，避免循环。

索引是类似 `locate/updatedb` 的持久快照，使用**自有 SQLite FTS5 trigram 格式**，不兼容 GNU locate / mlocate / plocate 的二进制数据库。至少 3 个 Unicode 字符的搜索使用倒排索引加速；较短输入直接扫描名称列。单次搜索限制 5 秒、最多 200 条，支持分页。文件变更在下次索引后反映，不进行实时文件系统监听。

重建在 SQLite WAL 事务内完成：完成后原子替换；取消或失败时回滚。某个根目录断开或不可访问会保留整份旧索引；子目录权限错误和扫描中消失的条目会被跳过并计数。索引只覆盖运行程序的用户可以访问的内容。Unix 上非 UTF-8 路径以有损 Unicode 展示。

## 终端

「本地终端」中的启动按钮创建一个真正的 PTY / Windows ConPTY 会话，支持颜色、交互程序、持续的工作目录与变量、Ctrl+C 和窗口大小同步。

- Windows 优先使用 PATH 中的 PowerShell 7，否则使用系统 Windows PowerShell。需要 Windows 10 1809 或更新系统。
- macOS / Linux 使用 `$SHELL` 指定的可用程序，否则 `/bin/sh`。
- 默认工作目录是当前用户主目录，可使用 `--shell` 指定其他 Shell。
- 全局最多 4 个并发 Shell。断开连接、会话到期、退出登录、服务器正常退出时，结束相应 Shell 并回收进程；主动 `nohup` / `disown` 等脱离会话的进程不保证终止。
- **密码授予当前操作系统用户的完整 Shell 权限**，不是沙箱。不要用管理员 / root 身份运行，除非确实需要这些权限。

## 远程访问与 HTTPS

默认仅绑定回环地址。跨机器访问可使用内置 TLS：

```powershell
rwebman.exe --bind 0.0.0.0:8443 --public-url https://files.example.com:8443 --tls-cert fullchain.pem --tls-key privkey.pem
```

证书与密钥使用 PEM，证书应由浏览器信任且覆盖访问域名。也可以让 HTTPS 反向代理连接到回环 HTTP：

```sh
./rwebman --bind 127.0.0.1:8080 --public-url https://files.example.com
```

代理必须保留浏览器使用的 **Host 和 Origin**，并支持 `/api/terminal` 的 WebSocket Upgrade。程序不信任任意 `X-Forwarded-*` 头。指定 `--public-url` 后，只接受这一精确来源；通过其他别名或地址访问会被拒绝。默认不指定时允许配置端口上的本机回环地址与 `localhost`。

如果明确接受明文连接的风险，可显式使用以下模式；密码和 Shell 数据均未加密，限可信隔离网络：

```powershell
rwebman.exe --bind 0.0.0.0:8080 --public-url http://192.168.1.10:8080 --allow-insecure-http
```

服务端防护包括 Argon2id 加盐哈希、随机 256 位会话 ID、HttpOnly / SameSite=Strict Cookie、HTTPS 下的 Secure Cookie、8 小时绝对过期、登录限速、哈希并发限制、精确 Host / Origin 校验、写请求 CSRF Token、WebSocket 子协议携带的 CSRF 校验、请求 / 消息大小限制和 CSP。刷新网页继续使用现有登录；重启服务后必须重新登录。锁定会立即撤销当前浏览器会话及其终端，其他浏览器的独立会话不受影响。

本地数据文件包含文件路径和密码哈希，需保护数据目录权限；Unix 自动限制为当前用户访问，Windows 默认使用当前用户 LocalAppData 的继承权限。自定义共享目录时请自行配置合适的 ACL。

## 数据与单文件发布

默认数据目录由操作系统决定：

- Windows：`%LOCALAPPDATA%\rwebman\data`
- macOS：`~/Library/Application Support/rwebman`
- Linux：`$XDG_DATA_HOME/rwebman`，未设置时为 `~/.local/share/rwebman`

其中保存 `config.json`、`index.sqlite3`、SQLite WAL 辅助文件和实例锁。**单文件发布指只需分发可执行文件**；运行产生的索引、配置和密码哈希仍需持久写到磁盘。TLS 模式另需用户自己的证书与密钥。

Windows 发行版启用静态 CRT，SQLite 和 TLS 实现也编译进程序；不依赖 OpenSSL DLL。macOS 依赖系统库。Linux 可以用 musl 构建静态版本：

```sh
rustup target add x86_64-unknown-linux-musl
# Debian / Ubuntu 编译机还需安装 musl-tools
cargo build --release --locked --target x86_64-unknown-linux-musl
```

仓库包含 Windows、Linux、macOS 的 GitHub Actions 测试与单文件构建工作流。每个平台分别构建，无法用同一个 EXE 跨操作系统运行。

## 开发与验证

Rust 1.88+；构建机需要相应 C 编译器以编译 bundled SQLite。无需 npm 步骤。

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

自动测试包含密码与会话撤销、登录限速、Host / Origin / CSRF 校验、根目录校验、中文 / 特殊字符 / 大小写搜索、取消与替换索引、设置持久化，以及真实 WebSocket → PTY 命令执行和退出登录后回收 Shell。PTY 测试仅运行自己的临时 Shell 与临时文件。

代码分层：`config.rs` 处理系统默认值与持久配置，`auth.rs` 管理鉴权，`index.rs` 建立与查询索引，`server.rs` 提供 HTTP API，`terminal.rs` 管理 PTY 生命周期，`web/` 保存内嵌前端。

上游文档：[Axum WebSocket](https://docs.rs/axum/latest/axum/extract/ws/)、[portable-pty](https://docs.rs/portable-pty/0.9.0/portable_pty/)、[SQLite FTS5](https://www.sqlite.org/fts5.html)、[xterm.js addons](https://xtermjs.org/docs/guides/using-addons/)。前端第三方授权见 `THIRD_PARTY_NOTICES.md`。

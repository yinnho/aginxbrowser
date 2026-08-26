# v0.2.0 Release — 构建矩阵与发布步骤（待用户按发布键）

Tag `v0.2.0` 已推（f5ff5cc）。release notes 草稿在 docs/release-v0.2.0.md。

## 构建矩阵

| Target | 平台 | 特性 | 说明 |
|---|---|---|---|
| aarch64-apple-darwin | macOS Apple Silicon | stealth,screenshot | 本机原生 |
| x86_64-apple-darwin | macOS Intel | stealth,screenshot | rustup target + cross |
| x86_64-unknown-linux-musl | Linux x86_64 静态 | stealth,screenshot | musl 全静态，服务器免依赖；⚠️BoringSSL/wreq 在 Linux 上才编译得过（macOS 上 C++ 头报错）|

产物命名：`aginxbrowser-v0.2.0-{target}.tar.gz`（内含二进制+README+install.md）

## 发布步骤（gh release create 时把 notes 正文换成 release-v0.2.0.md 内容）

1. 三平台构建 → 打包 tar.gz + sha256sum
2. `gh release create v0.2.0 --title "v0.2.0" --notes-file <(展开的 notes)` + 逐个上传资产
3. 发后动作：
   - smithery / MCP registry 版本号同步到 0.2.0
   - install.md 的 self-host 段加"直接下载二进制"路径
   - Trendshift/目录 PR 里如需可补 release 链接

## 已知取舍

- macOS 上构建 Linux musl 目标会踩 BoringSSL C++ 编译错误 → Linux 二进制用 86quan 服务器构建（musl linker 已配好 .cargo/config.toml）
- captcha_solver 默认 false（无 key），notes 已如实声明

<div align="center">

<img src="assets/logo.svg" alt="ColabDeck" width="80" height="80">

# ColabDeck (`colab-mcp`)

**在 Antigravity 中管理 Google Colab GPU，查看算力用量，并在主机和虚拟机上设置成本守卫。**

[English](README.md) | 简体中文 · [Releases](https://github.com/eatingDream-antigravity-plugins/ColabDeck/releases) · [AI 指南](docs/ai-guide.md) · [`llms.txt`](llms.txt)

[![CI](https://github.com/eatingDream-antigravity-plugins/ColabDeck/actions/workflows/ci.yml/badge.svg)](https://github.com/eatingDream-antigravity-plugins/ColabDeck/actions/workflows/ci.yml)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-1A1916?logo=rust&logoColor=white)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

</div>

## 它是什么

`colab-mcp` 是一个原生 Rust MCP 服务与 SEP-1865 交互式控制面板，面向官方 Google Colab CLI（`google-colab-cli`），用于 Antigravity 及各类 MCP Agent。

本机 `colab` CLI 负责身份认证和运行时管理。ColabDeck 提供成本守卫、SSH 端口转发和声明式虚拟机插件部署。

## 核心能力

- **面板缓存与标准输入输出**：面板工具立即返回缓存。服务在后台刷新用量和会话。面板 HTML 由静态资源提供。
- **ADC 与多账号隔离**：支持应用默认凭据（ADC）和账号专属的 OAuth2 用户凭据。运行时操作固定到指定账号和端点。
- **指定硬件与孤儿实例清理**：支持 CPU、GPU（`T4`/`L4`/`G4`/`A100`/`H100`）和 TPU（`v5e1`/`v6e1`），支持 `min_vram_mib` 自动重试抽卡（如 A100 80GB）及一键清理 `[?]` 浏览器未托管实例。
- **双层成本守卫**：VM 端独立 `watchdog.py`（硬截止 + 空闲停机，关闭面板仍生效）结合主机端跨账号时长/花费上限/余额底线监控，停机失败自动保留 `PendingStop` 重试。
- **独占式 SSH 端口转发**：通过 `colab ssh --proxy-mode` 绑定 `127.0.0.1`，不改写 `~/.ssh/config`，禁用保留端口 `8080`，转义 OpenSSH `%` 令牌，杜绝重连时静默新开计费实例。
- **声明式 VM 插件与一键 GGUF 部署**：内置 `llama-cpp` 插件，支持 Hugging Face GGUF 搜索、显存量化拟合、归档完整性校验与实时日志流。

## 快速开始

需要 Rust 1.89+（源码构建）、Node.js 20+，以及位于 `PATH`（或 `COLAB_MCP_COLAB_BIN`）中的 Google Colab CLI（`colab`）。

```sh
git clone https://github.com/eatingDream-antigravity-plugins/ColabDeck.git
cd ColabDeck
chmod +x scripts/install.sh scripts/check_sanitized.sh
./scripts/install.sh
```

使用零计费演示模式体验完整界面与流程：

```sh
colab-mcp serve --mock
```

## 给 AI Agent 的文档

全部 CLI 参数、环境变量、MCP 工具 Schema、SEP-1865 协商规则与成本安全流程均整理在 Agent 文档中：

```sh
npx skills add eatingDream-antigravity-plugins/ColabDeck
```

> 直接告诉你的 Agent：*「阅读 [`llms.txt`](llms.txt) 和 [`docs/ai-guide.md`](docs/ai-guide.md)，配置 `colab-mcp` 并在成本守卫保护下管理 Colab GPU 会话。」*

## 安全、隐私与免责声明

- **计费诚实原则**：关闭隧道或关闭面板**不会**停止 VM 计费；只有经后端确认的 `stop` 或 `reap_orphan` 才会结束计费，停机失败会保持 `PendingStop` 继续受守卫保护。
- **最小权限边界**：MCP 工具无法修改本机 `colab` 可执行文件路径（仅信任启动期 `COLAB_MCP_COLAB_BIN` 与标准路径）；插件脚本**仅**在远端 Colab VM 上运行；密钥不经命令行参数传递且不落盘。
- **零计费测试护栏**：单元测试与集成测试在编译期及运行期双重拒绝真实子进程执行，`cargo test` 绝不会调用真实 `colab` 或消耗算力单元。
- **Google Colab 服务条款免责声明**：本项目为独立开源工具，与 Google 无隶属关系。使用前请自行确认符合 Google 服务条款及 Colab Pro/Pro+ 使用政策。

## 文档索引

- [AI Agent 指南与完整参考 (`docs/ai-guide.md`)](docs/ai-guide.md) — MCP 工具、CLI 参数、环境变量与守卫流程
- [便携 Agent Skill (`skills/colab-panel/SKILL.md`)](skills/colab-panel/SKILL.md) · [LLM 索引 (`llms.txt`)](llms.txt)
- [安全策略](SECURITY.md) · [贡献指南](CONTRIBUTING.md) · [第三方声明](THIRD_PARTY_NOTICES.md)

## 致谢与许可证

本项目受 [scarletkc/colab-playground](https://github.com/scarletkc/colab-playground)（`Copyright (c) 2026 ScarletKc`，MIT 协议）启发并复用了其部分 VM 看门狗与插件清单设计。完整署名与许可证原文见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

基于 [MIT License](LICENSE) 开源。

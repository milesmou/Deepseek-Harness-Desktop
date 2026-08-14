# DeepSeek Harness (Tauri)

用 Tauri 2 把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的本地 Web GUI 包装成 Windows 桌面应用。工程只保留**网络引导版**:不在 exe 中内置 Node 或 dsh 依赖,首次启动时按需准备运行环境。

## 工作原理

1. 打开本地 loading 页;
2. 探测 `http://127.0.0.1:3080`,已有 Harness 服务时直接接入;
3. 系统同时存在可用的 Node 和 npm 时直接使用,否则自动安装便携 Node;
4. Node 并行测试中国大陆源与官网源的连接延迟;dsh 优先使用华为云,不可用时自动切换腾讯云;
5. 本地已安装 dsh 时立即启动 Web 服务,服务启动后再在后台查询最新版本;
6. 发现新版本时弹窗提示并保存待更新标记;可选择“立即重启更新”,也可选择“下次启动更新”;
7. 首次缺少 dsh 时仍会在启动阶段完成安装;
8. 应用退出时只回收本应用启动的服务进程。

读取到 dsh 版本后,窗口标题显示为 `DeepSeek Harness <dsh 版本>`。
窗口和 WebView 的初始背景色与 loading 页统一为 `#0b0f1a`,避免启动和重启时闪白。

## 下载源

- Node 中国大陆源:`https://npmmirror.com/mirrors/node/`
- Node 官网源:`https://nodejs.org/download/release/`
- dsh 华为云主源:`https://repo.huaweicloud.com/repository/npm/`
- dsh 腾讯云备用源:`https://mirrors.cloud.tencent.com/npm/`

Node 下载完成后使用官方 SHA-256 校验。首选源失败时自动切换备用源;dsh 默认使用华为云,华为云不可用、版本查询失败或安装失败时切换腾讯云。dsh 已安装但两个 registry 都无法查询时,直接使用本地版本。

## 安装目录

- 自动安装的便携 Node:`%APPDATA%\com.deepseek.harness.desktop\bootstrap\node-v24.16.0-win-x64\`
- dsh 与依赖:`%LOCALAPPDATA%\npm-cache\_npx\1e7f6d9597241db0\`（npm/npx 对 `@deepseek-ai/dsh` 生成的默认缓存目录）
- 诊断日志:`%APPDATA%\com.deepseek.harness.desktop\dsh-server.log`

日志达到 5 MiB 后轮转为 `dsh-server.log.1`,最多保留两份。

## 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `DSH_PORT` | `3080` | Harness 服务端口 |
| `DSH_WEB_COMMAND` | 空 | 自定义启动命令,优先级最高 |

## 开发与发布

前置要求:Node.js、Rust stable/MSVC 工具链、Windows WebView2。

```powershell
npm install
npm run tauri dev
npm run check
npm run lint
npm test

publish.cmd
```

`publish.cmd` 生成 `publish\DeepSeek Harness.exe`,不包含 Node 或 dsh 依赖。首次运行必须联网;已安装 dsh 时先启动服务,再在后台检查更新。发现新版本会弹窗,可立即重启更新或留到下次启动时更新。

## 说明

- 这是社区桌面包装壳,不是 DeepSeek 官方桌面客户端;
- 若本机已经运行 Harness GUI,桌面应用会直接复用该服务;
- 图标目前使用 Tauri 默认图标。

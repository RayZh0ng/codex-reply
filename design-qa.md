# Codex Relay 全应用设计 QA

## 验收范围

本轮视觉方向为“本机控制台 / 专业工具工作台”：以冷灰中性色为基础，使用单一蓝色强调色、克制的半透明应用壳、紧凑的桌面信息密度和稳定的语义状态色。验收覆盖以下现有产品范围，不引入新的业务能力：

- 工作台：受管任务编辑器、运行状态面板、基础设施摘要。
- 档案：搜索筛选、档案列表、状态与额度、档案操作。
- 网关：服务状态、监听配置、访问保护、Codex 切换与客户端 Key。
- 会话：项目主从视图、批量操作、来源明细与删除确认。
- 协作：飞书、QQ、企业微信、Discord、Telegram 五个平台及配置流程。
- 设置：主题、更新、环境检查和旧工作区清理。
- 全局与弹层：离线/错误提示、确认框、档案导入、协作指南、侧边栏抽屉及焦点归还。

## 2026-08-07 最终视觉收口

- 页面内容区改为高不透明度实体表面，磨砂与背景模糊只保留在应用壳、侧边栏、顶部工具栏和浮层。
- 工作台下方的基础设施、Relay 流量与资源入口已合并为同一分区表面，依靠分割线建立层级。
- 档案页已从双列卡片阵列改为统一列表；身份、有效性、套餐同步和网关状态在行内可见，额度与完整配置通过“档案详情”按需展开。
- 设置页的外观、更新、环境检查与旧工作区清理使用连续设置表面；六个页面的无意义英文眉题已移除或改为中文功能标签。
- 最终工作台预览使用与 IPC 类型一致的业务场景数据生成，不进入产品运行时或持久化数据：
  - `/tmp/codex-relay-ui-qa/dashboard-redesign-light-1440x900.jpg`
  - `/tmp/codex-relay-ui-qa/dashboard-redesign-dark-1440x900.jpg`
  - `/tmp/codex-relay-ui-qa/profiles-redesign-light-1440x900.jpg`
- `1024 × 720` 复验结果为 `scrollWidth === clientWidth === 1024`，侧边栏按设计切换为覆盖式导航，主操作与内容滚动保持可访问。

## 原生应用视觉检查

在重新启动的 Tauri 开发应用中检查了真实业务数据。原生业务页主要在约 `1152 × 768` 的桌面窗口中完成检查；Computer Use 截图可能包含窗口缩放后的画布空白，因此精确横向溢出结论以浏览器 viewport 检查为准。

### 浅色主题

| 页面   | 截图                                                      |
| ------ | --------------------------------------------------------- |
| 工作台 | `/tmp/codex-relay-ui-qa/dashboard-light-native.png`       |
| 档案   | `/tmp/codex-relay-ui-qa/profiles-light-native.png`        |
| 网关   | `/tmp/codex-relay-ui-qa/gateway-light-native.png`         |
| 会话   | `/tmp/codex-relay-ui-qa/sessions-light-native-fixed.jpeg` |
| 协作   | `/tmp/codex-relay-ui-qa/collaboration-light-native.png`   |
| 设置   | `/tmp/codex-relay-ui-qa/settings-light-native.png`        |

### 深色主题

| 页面   | 截图                                                     |
| ------ | -------------------------------------------------------- |
| 工作台 | `/tmp/codex-relay-ui-qa/dashboard-dark-native.png`       |
| 档案   | `/tmp/codex-relay-ui-qa/profiles-dark-native.png`        |
| 网关   | `/tmp/codex-relay-ui-qa/gateway-dark-native.png`         |
| 会话   | `/tmp/codex-relay-ui-qa/sessions-dark-native-fixed.jpeg` |
| 协作   | `/tmp/codex-relay-ui-qa/collaboration-dark-native.png`   |
| 设置   | `/tmp/codex-relay-ui-qa/settings-dark-native.png`        |

检查结果：六个页面在浅色与深色主题中使用一致的页面标题、工具栏、边框、圆角、列表密度、状态色和焦点语法。工作台保持任务编辑器为首要区域；档案的长邮箱和模型列表没有破坏统一列表布局；会话的长路径、大量会话与来源信息保持在限定区域内；设置的环境检查被压缩为总览和可展开列表。

深色会话页复验时发现统计数字与选中项目文字仍继承了旧的浅色硬编码颜色。最终在页面级样式中改用 `--color-text-primary` 与 `--color-text-secondary`，并重新检查深浅主题；修复后统计值、项目标题、路径和辅助信息均可清晰辨认。

## 弹窗与键盘交互

| 场景         | 截图                                                         | 结果                                                    |
| ------------ | ------------------------------------------------------------ | ------------------------------------------------------- |
| 档案导入     | `/tmp/codex-relay-ui-qa/profile-import-dark-native.png`      | 标题、说明、表单与底部操作层级清楚；Escape 正常关闭。   |
| 协作指南     | `/tmp/codex-relay-ui-qa/collaboration-guide-dark-native.png` | 大尺寸正文区域可独立滚动；Escape 正常关闭。             |
| 会话删除确认 | `/tmp/codex-relay-ui-qa/session-delete-dark-native.png`      | 删除范围与不可逆风险明确；未执行删除。                  |
| 侧边栏抽屉   | `/tmp/codex-relay-ui-qa/sidebar-drawer-dark-native.png`      | 覆盖式导航可用；Escape 关闭后焦点返回“展开侧边栏”按钮。 |
| 键盘焦点     | `/tmp/codex-relay-ui-qa/keyboard-focus-dark-native.png`      | Tab 可移动到“刷新状态”，焦点环在深色主题中清晰可见。    |

协作页确认五个平台切换器均可访问：飞书、QQ、企业微信、Discord、Telegram。未配置平台不会渲染虚假的机器人或会话内容，已配置区域使用紧凑列表呈现。

## 响应式与离线检查

通过本地 Web 应用精确设置 viewport，检查应用壳、覆盖式导航与离线降级状态。纯 Web 环境没有 Tauri runtime，因此页面按预期显示“本机运行时不可用”，该状态同时用于确认全局错误提示和窄窗口布局。

| Viewport      | `scrollWidth` | `clientWidth` | 结果                         |
| ------------- | ------------: | ------------: | ---------------------------- |
| `1024 × 720`  |          1024 |          1024 | 无横向溢出                   |
| `1280 × 800`  |          1280 |          1280 | 无横向溢出                   |
| `1440 × 960`  |          1440 |          1440 | 无横向溢出                   |
| `1600 × 1000` |          1600 |          1600 | 无横向溢出                   |
| `768 × 900`   |           768 |           768 | 无横向溢出，单列压力测试可用 |

补充截图：

- `/tmp/codex-relay-ui-qa/1024x720-browser-offline.png`
- `/tmp/codex-relay-ui-qa/768x900-browser-offline.png`

所有尺寸均报告 `overflow-x: hidden`，页面根节点的 `scrollWidth === clientWidth`。`1024–1179px` 使用紧凑图标栏与覆盖式完整导航；`768px` 压力测试下内容转为单列，主操作保持可访问。完成检查后已恢复测试 viewport 并关闭浏览器测试标签。

## 状态与内容稳定性

- 已检查原生业务数据中的长档案邮箱、模型列表、长项目路径、大量会话和较长环境错误文本。
- 已检查按钮禁用、进行中反馈、危险操作确认、离线提示和空工作区清理状态。
- Dialog 在忙碌状态下由共享组件阻止 Escape、背景关闭和重复提交；相关行为由共享 UI 测试覆盖。
- 所有动效继续使用全局 120–220ms 左右的反馈时长，并由全局 `prefers-reduced-motion` 规则关闭非必要动画。
- 未伪造任务日志、流量趋势、机器人、会话或其他业务数据。

## 已知范围外风险

原生应用环境检查仍报告 GUI 进程的 `PATH` 中找不到 Node.js 与 npm，但同一环境可以检测到 Codex CLI 与 Git。这是本轮之前已存在的运行环境探测问题；本次仅记录现状，没有修改 Rust 探测逻辑、IPC 数据结构、Tauri capability、CSP 或窗口配置。

## 结论

六个页面、关键弹窗、深浅主题、键盘焦点、覆盖式导航、离线降级和计划中的五组桌面尺寸均已完成实际检查。已修复检查中发现的深色会话页文字对比度问题；未发现仍阻断本轮 UI/UX 交付的视觉缺陷。

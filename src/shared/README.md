# Shared

仅放置多个功能稳定复用的 UI、工具和类型；业务专属代码应保留在对应的 `features` 目录。

`ui/` 对外提供稳定的 Button、StatusPill、FormField、Dialog、Select 与 Skeleton。业务页使用语义 variant/tone，不复制颜色、阴影或 focus 样式；新增一次性页面组件仍保留在对应 feature 中。

`theme.ts` 管理设备级 `system / light / dark` 偏好、系统主题监听和 Tauri 原生窗口同步；业务功能只消费主题状态，不直接读写 DOM 或本机存储。

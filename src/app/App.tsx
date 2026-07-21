import "./App.css";

function App() {
  return (
    <main className="app-shell">
      <section className="welcome-card" aria-labelledby="app-title">
        <p className="eyebrow">Tauri · React · TypeScript</p>
        <h1 id="app-title">桌面应用脚手架已就绪</h1>
        <p className="intro">
          从这里开始构建功能。前端代码使用 React，受限的系统能力与 Rust 代码位于{" "}
          <code>src-tauri</code>。
        </p>

        <ul className="foundation-list" aria-label="项目基础能力">
          <li>严格 TypeScript 与组件测试</li>
          <li>ESLint、Prettier 与 Git 提交校验</li>
          <li>最小 Tauri 权限与内容安全策略</li>
        </ul>

        <p className="status" role="status">
          尚未启用业务命令、文件访问或外部网络权限。
        </p>
      </section>
    </main>
  );
}

export default App;

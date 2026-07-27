import React from "react";
import ReactDOM from "react-dom/client";
import App from "./app/App";
import { detectDesktopPlatform } from "./shared/platform";
import { initialiseTheme } from "./shared/theme";
import "./styles/global.css";

document.documentElement.dataset.platform = detectDesktopPlatform(navigator.userAgent);
initialiseTheme();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

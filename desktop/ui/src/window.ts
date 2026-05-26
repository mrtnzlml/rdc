// Window-drag helpers. The `data-tauri-drag-region` HTML attribute is
// only honored when Tauri serves the index.html itself (it injects a
// JS handler at load). Our Vite dev server bypasses that injection, so
// the attribute is inert in dev mode. Calling `startDragging()`
// explicitly from a React onMouseDown handler works in both dev and
// prod.

import { getCurrentWindow } from "@tauri-apps/api/window";
import type { MouseEvent } from "react";

export function startWindowDrag(e: MouseEvent) {
  // Left button only — right/middle click should not initiate a drag.
  if (e.button === 0) {
    void getCurrentWindow().startDragging();
  }
}

export function toggleWindowMaximize() {
  void getCurrentWindow().toggleMaximize();
}

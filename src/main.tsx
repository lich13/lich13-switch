import React from "react";
import { createRoot } from "react-dom/client";

const root = createRoot(document.getElementById("root")!);
const isQuickPanel =
  new URLSearchParams(location.search).get("panel") === "quick";

void (isQuickPanel ? import("./QuickEntry") : import("./AppEntry")).then(
  ({ default: Entry }) => {
    root.render(
      <React.StrictMode>
        <Entry />
      </React.StrictMode>,
    );
  },
);

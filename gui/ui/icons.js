"use strict";

// The app's one icon set: 16 × 16 line icons drawn in currentColor with a
// 1.5 px stroke, built as SVG elements (the CSP allows no inline markup
// strings and no icon fonts). `icon(name)` returns a fresh <svg>.
(function (root) {
  const P = {
    folder: "M1.75 4.25v8.5h12.5v-7h-6.5l-1.5-1.5h-4.5z",
    file: "M3.75 1.75h5.5l3 3v9.5h-8.5zM9.25 1.75v3h3",
    drive: "M1.75 9.25l1.5-6.5h9.5l1.5 6.5v4h-12.5zM1.75 9.25h12.5M11.25 11.25h.5",
    usb: "M5.75 1.75h4.5v4h-4.5zM4.25 5.75h7.5v8.5h-7.5zM7 3.5v.5M9 3.5v.5",
    network: "M8 1.75a6.25 6.25 0 1 0 0 12.5 6.25 6.25 0 1 0 0-12.5zM1.75 8h12.5M8 1.75c2 2 2 10.5 0 12.5M8 1.75c-2 2-2 10.5 0 12.5",
    copies: "M5.25 5.25h8v8h-8zM2.75 10.75v-8h8",
    trash: "M2.25 4.25h11.5M6.25 4.25v-2.5h3.5v2.5M3.75 4.25l.75 10h7l.75-10M6.75 7v4.5M9.25 7v4.5",
    refresh: "M13.25 8a5.25 5.25 0 1 1-1.54-3.71M13.25 2.25v3.5h-3.5",
    back: "M13.25 8h-10.5M6.75 4l-4 4 4 4",
    up: "M8 13.25v-10.5M4 6.75l4-4 4 4",
    gear: "M8 5.75a2.25 2.25 0 1 0 0 4.5 2.25 2.25 0 1 0 0-4.5zM8 1.25v2M8 12.75v2M1.25 8h2M12.75 8h2M3.2 3.2l1.4 1.4M11.4 11.4l1.4 1.4M3.2 12.8l1.4-1.4M11.4 4.6l1.4-1.4",
    close: "M3.75 3.75l8.5 8.5M12.25 3.75l-8.5 8.5",
    stop: "M4.25 4.25h7.5v7.5h-7.5z",
    compare: "M2.25 5.25h10.5M9.75 2.25l3 3-3 3M13.75 10.75h-10.5M6.25 7.75l-3 3 3 3",
    alert: "M8 1.75l6.5 12h-13zM8 6.25v3.5M8 11.75v.5",
    check: "M2.75 8.5l3.5 3.5 7-8",
    lock: "M3.75 7.25h8.5v6.5h-8.5zM5.25 7.25v-2.5a2.75 2.75 0 0 1 5.5 0v2.5",
    clock: "M8 1.75a6.25 6.25 0 1 0 0 12.5 6.25 6.25 0 1 0 0-12.5zM8 4.25v4l2.5 1.5",
    queue: "M2.25 4h11.5M2.25 8h11.5M2.25 12h7",
    scan: "M7 2.25a4.75 4.75 0 1 0 0 9.5 4.75 4.75 0 1 0 0-9.5zM10.5 10.5l3.25 3.25",
    plus: "M8 2.75v10.5M2.75 8h10.5",
  };
  const NS = "http://www.w3.org/2000/svg";
  function icon(name, label) {
    const svg = document.createElementNS(NS, "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    svg.setAttribute("class", `ic ic-${name}`);
    if (label) {
      svg.setAttribute("role", "img");
      svg.setAttribute("aria-label", label);
    } else {
      svg.setAttribute("aria-hidden", "true");
    }
    const path = document.createElementNS(NS, "path");
    path.setAttribute("d", P[name] || P.file);
    svg.append(path);
    return svg;
  }
  root.icon = icon;
  root.ICON_NAMES = Object.keys(P);
})(globalThis);

import sharp from "sharp";
import { mkdir, readFile, writeFile, copyFile } from "node:fs/promises";
const sizes = [16, 22, 32, 48, 64, 128, 256, 512, 1024];
await mkdir("assets/png", { recursive: true });
await mkdir("src-tauri/icons", { recursive: true });
await mkdir("public", { recursive: true });
const svg = await readFile("assets/icon.svg");
const tray = await readFile("assets/tray.svg", "utf8");
for (const size of sizes) {
  await sharp(svg)
    .resize(size, size)
    .png()
    .toFile(`assets/png/icon-${size}.png`);
  for (const [color, hex] of [
    ["black", "#000"],
    ["white", "#fff"],
  ])
    await sharp(Buffer.from(tray.replaceAll("#000", hex)))
      .resize(size, size)
      .png()
      .toFile(`assets/png/tray-${color}-${size}.png`);
}
for (const [src, dst] of [
  ["icon-32", "32x32"],
  ["icon-128", "128x128"],
  ["icon-256", "128x128@2x"],
  ["icon-1024", "icon"],
  ["tray-black-22", "tray"],
  ["tray-black-32", "tray-black"],
  ["tray-white-32", "tray-white"],
])
  await copyFile(`assets/png/${src}.png`, `src-tauri/icons/${dst}.png`);
await copyFile("assets/icon.svg", "public/icon.svg");
// PNG-compressed ICO entries, supported by Windows Vista and later.
const icoSizes = [16, 32, 48, 64, 128, 256];
const images = await Promise.all(
  icoSizes.map((s) => readFile(`assets/png/icon-${s}.png`)),
);
const head = Buffer.alloc(6 + 16 * images.length);
head.writeUInt16LE(1, 2);
head.writeUInt16LE(images.length, 4);
let offset = head.length;
images.forEach((data, i) => {
  const p = 6 + 16 * i,
    s = icoSizes[i];
  head[p] = s === 256 ? 0 : s;
  head[p + 1] = head[p];
  head.writeUInt16LE(1, p + 4);
  head.writeUInt16LE(32, p + 6);
  head.writeUInt32LE(data.length, p + 8);
  head.writeUInt32LE(offset, p + 12);
  offset += data.length;
});
await writeFile("src-tauri/icons/icon.ico", Buffer.concat([head, ...images]));
const chunks = [];
for (const [type, size] of [
  ["icp4", 16],
  ["icp5", 32],
  ["icp6", 64],
  ["ic07", 128],
  ["ic08", 256],
  ["ic09", 512],
  ["ic10", 1024],
]) {
  const data = await readFile(`assets/png/icon-${size}.png`),
    h = Buffer.alloc(8);
  h.write(type);
  h.writeUInt32BE(data.length + 8, 4);
  chunks.push(h, data);
}
const h = Buffer.alloc(8);
h.write("icns");
h.writeUInt32BE(8 + chunks.reduce((s, b) => s + b.length, 0), 4);
await writeFile("src-tauri/icons/icon.icns", Buffer.concat([h, ...chunks]));
console.log("Vela Pulsar: SVG, 27 PNGs, ICNS and ICO generated");

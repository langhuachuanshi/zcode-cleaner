// 生成 1024x1024 极简图标：蓝底圆角方块 + 白色扫帚
const zlib = require('zlib'), fs = require('fs');
const W = 1024, R = 200;
const px = Buffer.alloc(W * W * 4);
function set(x, y, r, g, b, a = 255) {
  const i = (y * W + x) * 4;
  const na = a / 255, oa = px[i + 3] / 255;
  px[i] = r * na + px[i] * (1 - na); px[i+1] = g * na + px[i+1] * (1 - na);
  px[i+2] = b * na + px[i+2] * (1 - na); px[i+3] = Math.max(a, px[i+3]);
}
function inRounded(x, y) {
  const cx = Math.min(Math.max(x, R), W - R), cy = Math.min(Math.max(y, R), W - R);
  return (x - cx) ** 2 + (y - cy) ** 2 <= R * R || (x >= R && x <= W - R) || (y >= R && y <= W - R);
}
for (let y = 0; y < W; y++) for (let x = 0; x < W; x++) {
  if (!inRounded(x, y)) continue;
  const t = y / W; // 垂直渐变
  set(x, y, 0x4f + (0x3b - 0x4f) * t | 0, 0x8c + (0x6f - 0x8c) * t | 0, 0xff + (0xd4 - 0xff) * t | 0);
}
// 扫帚：竖直手柄 + 梯形帚头 + 帚毛
const cxm = 512;
for (let y = 150; y < 560; y++) for (let x = cxm - 30; x <= cxm + 30; x++) set(x, y, 245, 247, 250);       // 手柄
for (let y = 560; y < 730; y++) {                                                                          // 梯形头
  const w = 80 + (y - 560) / 170 * 130;
  for (let x = cxm - w; x <= cxm + w; x++) set(x, y, 235, 240, 246);
}
for (let y = 730; y < 860; y++) for (let x = cxm - 160; x <= cxm + 160; x += 8)                            // 帚毛(条纹)
  for (let k = 0; k < 5 && x + k < W; k++) set(x + k, y, 235, 240, 246);
// PNG 编码
const crcT = []; for (let n = 0; n < 256; n++) { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1; crcT[n] = c >>> 0; }
const crc32 = b => { let c = 0xFFFFFFFF; for (const v of b) c = crcT[(c ^ v) & 0xFF] ^ (c >>> 8); return (c ^ 0xFFFFFFFF) >>> 0; };
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}
const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(W, 0); ihdr.writeUInt32BE(W, 4);
ihdr[8] = 8; ihdr[9] = 6; // 8bit RGBA
const raw = Buffer.alloc((W * 4 + 1) * W);
for (let y = 0; y < W; y++) { raw[y * (W * 4 + 1)] = 0; px.copy(raw, y * (W * 4 + 1) + 1, y * W * 4, (y + 1) * W * 4); }
const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0)),
]);
fs.writeFileSync('scripts/app-icon.png', png);
console.log('图标已生成', png.length, 'bytes');

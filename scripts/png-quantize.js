// PNG 量化器(仅限本仓库资产): 8bit RGB/RGBA 非隔行 → 256 色调色板 PNG。
// 自校验: 输出先解码回 RGB 与原图逐像素比对, 完全一致才落盘; 颜色超 256 色、
// 带有效透明通道或量化后未变小则放弃(不破坏原图)。
// 用法: node scripts/png-quantize.js <png路径>
const fs = require("fs");
const zlib = require("zlib");

function crc32(buf) {
  let table = crc32.table;
  if (!table) {
    table = crc32.table = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      table[n] = c;
    }
  }
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = table[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function decodeRGB(file) {
  const b = fs.readFileSync(file);
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  if (!b.subarray(0, 8).equals(sig)) throw new Error("非 PNG");
  let off = 8, w, h, bd, ct, il;
  const idat = [];
  while (off < b.length) {
    const len = b.readUInt32BE(off);
    const type = b.toString("ascii", off + 4, off + 8);
    const data = b.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") { w = data.readUInt32BE(0); h = data.readUInt32BE(4); bd = data[8]; ct = data[9]; il = data[12]; }
    else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    off += 12 + len;
  }
  if (bd !== 8 || (ct !== 2 && ct !== 6) || il !== 0) throw new Error(`不支持 bd=${bd} ct=${ct} il=${il}`);
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const bpp = ct === 6 ? 4 : 3;
  const stride = w * bpp;
  const px = Buffer.alloc(h * stride);
  let pos = 0;
  for (let y = 0; y < h; y++) {
    const f = raw[pos++];
    const line = raw.subarray(pos, pos + stride); pos += stride;
    const prev = y > 0 ? px.subarray((y - 1) * stride, y * stride) : Buffer.alloc(stride);
    const cur = px.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? cur[x - bpp] : 0;
      const bb = prev[x];
      const c = x >= bpp ? prev[x - bpp] : 0;
      let v = line[x];
      if (f === 1) v += a;
      else if (f === 2) v += bb;
      else if (f === 3) v += (a + bb) >> 1;
      else if (f === 4) {
        const p = a + bb - c, pa = Math.abs(p - a), pb = Math.abs(p - bb), pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? bb : c;
      }
      cur[x] = v & 0xff;
    }
  }
  if (bpp === 4) {
    for (let i = 3; i < px.length; i += 4) if (px[i] < 255) throw new Error("含有效透明通道, 不处理");
  }
  return { w, h, bpp, px };
}

function quantize(img) {
  const { w, h, bpp, px } = img;
  const map = new Map();
  const idx = Buffer.alloc(w * h);
  let n = 0;
  for (let i = 0; i < w * h; i++) {
    const key = (px[i * bpp] << 16) | (px[i * bpp + 1] << 8) | px[i * bpp + 2];
    let id = map.get(key);
    if (id === undefined) {
      if (n >= 256) throw new Error(`颜色数超 256 (${n})`);
      id = n++;
      map.set(key, id);
    }
    idx[i] = id;
  }
  const pal = Buffer.alloc(n * 3);
  for (const [key, id] of map) {
    pal[id * 3] = (key >> 16) & 255;
    pal[id * 3 + 1] = (key >> 8) & 255;
    pal[id * 3 + 2] = key & 255;
  }
  return { idx, pal, count: n };
}

function encode(w, h, idx, pal) {
  const raw = Buffer.alloc(h * (w + 1));
  for (let y = 0; y < h; y++) {
    raw[y * (w + 1)] = 0;
    idx.copy(raw, y * (w + 1) + 1, y * w, (y + 1) * w);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const t = Buffer.from(type, "ascii");
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(Buffer.concat([t, data])));
    return Buffer.concat([len, t, data, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; ihdr[9] = 3; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  return Buffer.concat([sig, chunk("IHDR", ihdr), chunk("PLTE", pal), chunk("IDAT", idat), chunk("IEND", Buffer.alloc(0))]);
}

// 解码调色板 PNG 回 RGB(回验用)
function decodePalRGB(buf) {
  let off = 8, w, h;
  const idat = [];
  let pal = null;
  while (off < buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") { w = data.readUInt32BE(0); h = data.readUInt32BE(4); }
    else if (type === "PLTE") pal = data;
    else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    off += 12 + len;
  }
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const rgb = Buffer.alloc(w * h * 3);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const id = raw[y * (w + 1) + 1 + x];
      rgb[(y * w + x) * 3] = pal[id * 3];
      rgb[(y * w + x) * 3 + 1] = pal[id * 3 + 1];
      rgb[(y * w + x) * 3 + 2] = pal[id * 3 + 2];
    }
  }
  return { w, h, rgb };
}

const src = process.argv[2];
if (!src) { console.error("用法: node scripts/png-quantize.js <png路径>"); process.exit(1); }
const img = decodeRGB(src);
const { idx, pal, count } = quantize(img);
const cand = encode(img.w, img.h, idx, pal);
const back = decodePalRGB(cand);
for (let i = 0; i < img.w * img.h * 3; i++) {
  if (back.rgb[i] !== img.px[i * img.bpp]) throw new Error(`回验失败 @像素${Math.floor(i / 3)}`);
}
const before = fs.statSync(src).size;
if (cand.length >= before) throw new Error("量化后未变小, 不替换");
fs.writeFileSync(src, cand);
console.log(`OK ${img.w}x${img.h} 颜色数=${count} ${before}B -> ${cand.length}B (-${Math.round((1 - cand.length / before) * 100)}%)`);

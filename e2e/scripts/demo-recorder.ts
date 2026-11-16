import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { Page } from '@playwright/test';
import { PNG } from 'pngjs';
import gifenc from 'gifenc';

const { GIFEncoder, quantize, applyPalette } = gifenc;

interface RecordedFrame {
  filePath: string;
  delayMs: number;
}

/**
 * Downscales an RGBA buffer by integer/bilinear sampling to keep the resulting
 * README GIF crisp while staying compact (< 3 MB).
 */
function resizeRgba(
  src: Uint8Array,
  srcW: number,
  srcH: number,
  dstW: number,
  dstH: number,
): Uint8Array {
  if (srcW === dstW && srcH === dstH) {
    return src;
  }
  const dst = new Uint8Array(dstW * dstH * 4);
  const xRatio = srcW / dstW;
  const yRatio = srcH / dstH;

  for (let y = 0; y < dstH; y++) {
    const sy = Math.min(srcH - 1, Math.floor(y * yRatio));
    for (let x = 0; x < dstW; x++) {
      const sx = Math.min(srcW - 1, Math.floor(x * xRatio));
      const si = (sy * srcW + sx) * 4;
      const di = (y * dstW + x) * 4;
      dst[di] = src[si];
      dst[di + 1] = src[si + 1];
      dst[di + 2] = src[si + 2];
      dst[di + 3] = src[si + 3];
    }
  }
  return dst;
}

export class DemoRecorder {
  private readonly enabled: boolean;
  private readonly framesDir: string;
  private readonly frames: RecordedFrame[] = [];
  private counter = 0;

  constructor(enabled: boolean) {
    this.enabled = enabled;
    this.framesDir = enabled ? fs.mkdtempSync(path.join(os.tmpdir(), 'keel-demo-frames-')) : '';
  }

  async capture(page: Page, label: string, delayMs = 800): Promise<void> {
    if (!this.enabled) return;
    this.counter += 1;
    const safeLabel = label.replace(/[^a-z0-9_-]+/gi, '-').toLowerCase();
    const filePath = path.join(
      this.framesDir,
      `${String(this.counter).padStart(4, '0')}-${safeLabel}.png`,
    );
    await page.screenshot({ path: filePath, animations: 'allow' });
    this.frames.push({ filePath, delayMs });
  }

  async burst(
    page: Page,
    label: string,
    count = 3,
    intervalMs = 250,
    frameDelayMs = 250,
  ): Promise<void> {
    if (!this.enabled) return;
    for (let i = 0; i < count; i++) {
      await this.capture(page, `${label}-${i + 1}`, frameDelayMs);
      if (i < count - 1) {
        await page.waitForTimeout(intervalMs);
      }
    }
  }

  async finish(outputGifPath: string, targetWidth = 960): Promise<void> {
    if (!this.enabled || this.frames.length === 0) return;

    const firstPng = PNG.sync.read(fs.readFileSync(this.frames[0].filePath));
    const dstW = Math.min(targetWidth, firstPng.width);
    const dstH = Math.round((firstPng.height * dstW) / firstPng.width);

    const gif = GIFEncoder();
    for (const frame of this.frames) {
      const png = PNG.sync.read(fs.readFileSync(frame.filePath));
      const rgba = resizeRgba(png.data, png.width, png.height, dstW, dstH);
      const palette = quantize(rgba, 128);
      const index = applyPalette(rgba, palette);
      gif.writeFrame(index, dstW, dstH, {
        palette,
        delay: frame.delayMs,
        repeat: 0,
      });
    }
    gif.finish();

    fs.mkdirSync(path.dirname(outputGifPath), { recursive: true });
    fs.writeFileSync(outputGifPath, gif.bytes());
    fs.rmSync(this.framesDir, { recursive: true, force: true });
    console.log(
      `[demo-recorder] Wrote ${this.frames.length} frames (${dstW}x${dstH}) to ${outputGifPath}`,
    );
  }
}

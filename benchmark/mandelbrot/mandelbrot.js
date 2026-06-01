// Mandelbrot set benchmark for JavaScript
// Counts total iterations across a grid of points
// Real-world use case: fractal rendering, GPU benchmarks
//
// Reports throughput (pixels rendered per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-mandelbrot
//
// Or manually:
//   node benchmark/mandelbrot/mandelbrot.js

const TARGET_NS = 1_000_000_000; // ~1s budget

function nowNs() {
  return Math.round(performance.now() * 1e6);
}

function nextIters(n, elapsed, target) {
  const e = elapsed > 0 ? elapsed : 1;
  let est = Math.floor((n * target) / e);
  const hi = n * 100;
  if (est > hi) est = hi;
  if (est > 1_000_000_000) est = 1_000_000_000;
  if (est < 1) est = 1;
  return est;
}

function printThroughput(workPerIter, n, elapsedNs, unit) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (workPerIter * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (unit === "B") {
    if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} GB/s`;
    else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} MB/s`;
    else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} KB/s`;
    else rbuf = `${rate.toFixed(2)} B/s`;
  } else {
    if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} G ${unit}/s`;
    else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} M ${unit}/s`;
    else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} k ${unit}/s`;
    else rbuf = `${rate.toFixed(2)} ${unit}/s`;
  }
  console.log(`Throughput: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

function mandelbrotIterations(cx, cy, maxIter) {
  let x = 0.0;
  let y = 0.0;
  let iter = 0;

  while (iter < maxIter) {
    const x2 = x * x;
    const y2 = y * y;

    if (x2 + y2 > 4.0) {
      return iter;
    }

    const xy = x * y;
    x = x2 - y2 + cx;
    y = 2.0 * xy + cy;
    iter += 1;
  }

  return maxIter;
}

function mandelbrotTotal(width, height, maxIter) {
  // Mandelbrot region: x in [-2.5, 1.0], y in [-1.0, 1.0]
  const xMin = -2.5;
  const xMax = 1.0;
  const yMin = -1.0;
  const yMax = 1.0;

  const dx = (xMax - xMin) / width;
  const dy = (yMax - yMin) / height;

  let totalIterations = 0;

  for (let py = 0; py < height; py++) {
    const cy = yMin + py * dy;

    for (let px = 0; px < width; px++) {
      const cx = xMin + px * dx;
      totalIterations += mandelbrotIterations(cx, cy, maxIter);
    }
  }

  return totalIterations;
}

function main() {
  const width = 1024;
  const height = 768;
  const maxIter = 256;

  // Warmup.
  let total = mandelbrotTotal(width, height, maxIter);

  let n = 1;
  let elapsed = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < n; i++) {
      total = mandelbrotTotal(width, height, maxIter);
    }
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(n, elapsed, TARGET_NS);
    if (nx <= n) break;
    n = nx;
  }

  console.log(`Mandelbrot ${width}x${height}, max_iter=${maxIter}`);
  console.log(`Total iterations: ${total}`);
  printThroughput(width * height, n, elapsed, "px");
}

main();

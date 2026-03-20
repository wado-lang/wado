// Mandelbrot set benchmark for JavaScript
// Counts total iterations across a grid of points
// Real-world use case: fractal rendering, GPU benchmarks
//
// How to run:
//   mise run benchmark-mandelbrot
//
// Or manually:
//   node benchmark/mandelbrot.js

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

function main() {
  const width = 1024;
  const height = 768;
  const maxIter = 256;

  // Mandelbrot region: x in [-2.5, 1.0], y in [-1.0, 1.0]
  const xMin = -2.5;
  const xMax = 1.0;
  const yMin = -1.0;
  const yMax = 1.0;

  const dx = (xMax - xMin) / width;
  const dy = (yMax - yMin) / height;

  const start = performance.now();

  let totalIterations = 0;

  for (let py = 0; py < height; py++) {
    const cy = yMin + py * dy;

    for (let px = 0; px < width; px++) {
      const cx = xMin + px * dx;
      const iter = mandelbrotIterations(cx, cy, maxIter);
      totalIterations += iter;
    }
  }

  const elapsed = performance.now() - start;

  console.log(`Mandelbrot ${width}x${height}, max_iter=${maxIter}`);
  console.log(`Total iterations: ${totalIterations}`);
  console.log(`Elapsed: ${elapsed.toFixed(2)} ms`);
}

main();

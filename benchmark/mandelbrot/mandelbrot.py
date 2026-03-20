#!/usr/bin/env python3
# Mandelbrot set benchmark for Python
# Counts total iterations across a grid of points
# Real-world use case: fractal rendering, GPU benchmarks
#
# How to run:
#   mise run benchmark-mandelbrot
#
# Or manually:
#   python3 benchmark/mandelbrot.py

import time


def mandelbrot_iterations(cx: float, cy: float, max_iter: int) -> int:
    x = 0.0
    y = 0.0
    iter = 0

    while iter < max_iter:
        x2 = x * x
        y2 = y * y

        if x2 + y2 > 4.0:
            return iter

        xy = x * y
        x = x2 - y2 + cx
        y = 2.0 * xy + cy
        iter += 1

    return max_iter


def main():
    width = 1024
    height = 768
    max_iter = 256

    # Mandelbrot region: x in [-2.5, 1.0], y in [-1.0, 1.0]
    x_min = -2.5
    x_max = 1.0
    y_min = -1.0
    y_max = 1.0

    dx = (x_max - x_min) / width
    dy = (y_max - y_min) / height

    start = time.perf_counter()

    total_iterations = 0

    for py in range(height):
        cy = y_min + py * dy

        for px in range(width):
            cx = x_min + px * dx
            iter = mandelbrot_iterations(cx, cy, max_iter)
            total_iterations += iter

    elapsed = (time.perf_counter() - start) * 1000

    print(f"Mandelbrot {width}x{height}, max_iter={max_iter}")
    print(f"Total iterations: {total_iterations}")
    print(f"Elapsed: {elapsed:.2f} ms")


if __name__ == "__main__":
    main()

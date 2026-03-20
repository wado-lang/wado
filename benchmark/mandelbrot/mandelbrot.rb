#!/usr/bin/env ruby
# Mandelbrot set benchmark for Ruby
# Counts total iterations across a grid of points
# Real-world use case: fractal rendering, GPU benchmarks
#
# How to run:
#   mise run benchmark-mandelbrot
#
# Or manually:
#   ruby benchmark/mandelbrot.rb

def mandelbrot_iterations(cx, cy, max_iter)
  x = 0.0
  y = 0.0
  iter = 0

  while iter < max_iter
    x2 = x * x
    y2 = y * y

    return iter if x2 + y2 > 4.0

    xy = x * y
    x = x2 - y2 + cx
    y = 2.0 * xy + cy
    iter += 1
  end

  max_iter
end

def main
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

  start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)

  total_iterations = 0

  height.times do |py|
    cy = y_min + py * dy

    width.times do |px|
      cx = x_min + px * dx
      iter = mandelbrot_iterations(cx, cy, max_iter)
      total_iterations += iter
    end
  end

  elapsed_ns = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
  elapsed_ms = elapsed_ns / 1_000_000

  puts "Mandelbrot #{width}x#{height}, max_iter=#{max_iter}"
  puts "Total iterations: #{total_iterations}"
  puts "Elapsed: #{elapsed_ms} ms"
end

main

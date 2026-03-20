#!/usr/bin/env ruby
# Prime counting benchmark for Ruby
# Counts primes up to LIMIT using trial division
# Reference: π(10,000,000) = 664,579
#
# How to run:
#   mise run benchmark-count-prime
#
# Or manually:
#   ruby benchmark/count_prime.rb

def prime?(n)
  return false if n < 2

  d = 2
  while d * d <= n
    return false if n % d == 0
    d += 1
  end
  true
end

def count_primes(limit)
  count = 0
  (2..limit).each do |n|
    count += 1 if prime?(n)
  end
  count
end

def main
  limit = 10_000_000

  start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  count = count_primes(limit)
  elapsed_ns = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
  elapsed_ms = elapsed_ns / 1_000_000

  puts "Prime count up to #{limit}: #{count}"
  puts "Elapsed: #{elapsed_ms} ms"
end

main

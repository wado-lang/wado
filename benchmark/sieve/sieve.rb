#!/usr/bin/env ruby
# Sieve of Eratosthenes benchmark for Ruby
# Counts primes up to LIMIT using the sieve algorithm
# Reference: π(10,000,000) = 664,579
#
# How to run:
#   mise run benchmark-sieve
#
# Or manually:
#   ruby benchmark/sieve.rb

def sieve_count(limit)
  # Use array of booleans
  is_prime = Array.new(limit + 1, true)

  # 0 and 1 are not prime
  is_prime[0] = false
  is_prime[1] = false

  # Sieve: mark multiples of each prime as not prime
  p = 2
  while p * p <= limit
    if is_prime[p]
      # Mark all multiples of p starting from p*p
      multiple = p * p
      while multiple <= limit
        is_prime[multiple] = false
        multiple += p
      end
    end
    p += 1
  end

  # Count primes
  is_prime.count(true)
end

def main
  limit = 10_000_000

  start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  count = sieve_count(limit)
  elapsed_ns = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
  elapsed_ms = elapsed_ns / 1_000_000

  puts "Sieve prime count up to #{limit}: #{count}"
  puts "Elapsed: #{elapsed_ms} ms"
end

main

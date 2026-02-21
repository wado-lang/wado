#!/usr/bin/env python3
# Sieve of Eratosthenes benchmark for Python
# Counts primes up to LIMIT using the sieve algorithm
# Reference: π(10,000,000) = 664,579
#
# How to run:
#   make benchmark-sieve
#
# Or manually:
#   python3 benchmark/sieve.py

import time


def sieve_count(limit: int) -> int:
    # Use bytearray for better performance than list of bools
    is_prime = bytearray(limit + 1)
    for i in range(limit + 1):
        is_prime[i] = 1

    # 0 and 1 are not prime
    is_prime[0] = 0
    is_prime[1] = 0

    # Sieve: mark multiples of each prime as not prime
    p = 2
    while p * p <= limit:
        if is_prime[p]:
            # Mark all multiples of p starting from p*p
            for multiple in range(p * p, limit + 1, p):
                is_prime[multiple] = 0
        p += 1

    # Count primes
    count = sum(is_prime)

    return count


def main():
    limit = 10000000

    start = time.perf_counter_ns()
    count = sieve_count(limit)
    end = time.perf_counter_ns()

    elapsed_ns = end - start
    elapsed_ms = elapsed_ns // 1000000

    print(f"Sieve prime count up to {limit}: {count}")
    print(f"Elapsed: {elapsed_ms} ms")


if __name__ == "__main__":
    main()

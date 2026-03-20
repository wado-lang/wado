#!/usr/bin/env python3
# Prime counting benchmark for Python
# Counts primes up to LIMIT using trial division
# Reference: pi(10,000,000) = 664,579
#
# How to run:
#   mise run benchmark-count-prime
#
# Or manually:
#   python3 benchmark/count_prime.py

import time


def is_prime(n: int) -> bool:
    if n < 2:
        return False
    d = 2
    while d * d <= n:
        if n % d == 0:
            return False
        d += 1
    return True


def count_primes(limit: int) -> int:
    count = 0
    for n in range(2, limit + 1):
        if is_prime(n):
            count += 1
    return count


def main():
    limit = 10000000

    start = time.perf_counter()
    count = count_primes(limit)
    elapsed = (time.perf_counter() - start) * 1000

    print(f"Prime count up to {limit}: {count}")
    print(f"Elapsed: {elapsed:.2f} ms")


if __name__ == "__main__":
    main()

// Minimal WASI P3 clocks shim for jco-transpiled Wado programs

export const monotonicClock = {
  now() {
    // Return nanoseconds as BigInt
    const [sec, nsec] = process.hrtime();
    return BigInt(sec) * 1000000000n + BigInt(nsec);
  },
  resolution() {
    return 1n; // 1 nanosecond
  },
  subscribeInstant(_when) {
    return { tag: "ok" };
  },
  subscribeDuration(_duration) {
    return { tag: "ok" };
  },
};

export const wallClock = {
  now() {
    const ms = Date.now();
    return {
      seconds: BigInt(Math.floor(ms / 1000)),
      nanoseconds: (ms % 1000) * 1000000,
    };
  },
  resolution() {
    return {
      seconds: 0n,
      nanoseconds: 1000000, // 1ms
    };
  },
};

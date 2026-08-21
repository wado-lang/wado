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
    // Keeps the sub-millisecond fraction `Date.now()` truncates; UUID v7 needs it.
    const ms = performance.timeOrigin + performance.now();
    const seconds = Math.floor(ms / 1000);
    const nanoseconds = Math.min(999999999, Math.floor((ms - seconds * 1000) * 1000000));
    return { seconds: BigInt(seconds), nanoseconds };
  },
  resolution() {
    return {
      seconds: 0n,
      nanoseconds: 1000, // ~1us; browsers clamp this far coarser
    };
  },
};

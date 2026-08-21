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
    // `Date.now()` fixes the millisecond, so the clock still follows system
    // time; the high-resolution clock only fills in the fraction it truncates.
    const ms = Date.now();
    const fraction = (performance.timeOrigin + performance.now()) % 1;
    const seconds = Math.floor(ms / 1000);
    const nanoseconds = (ms % 1000) * 1000000 + Math.floor(fraction * 1000000);
    return { seconds: BigInt(seconds), nanoseconds };
  },
  resolution() {
    return {
      seconds: 0n,
      nanoseconds: 1000, // ~1us; browsers clamp this far coarser
    };
  },
};

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

// The high-resolution clock has sub-millisecond precision but runs from process
// start, so anchor it to `Date.now()` and re-anchor once the two diverge.
let anchor = null;

export const wallClock = {
  now() {
    const elapsed = performance.timeOrigin + performance.now();
    const system = Date.now();
    let ms = anchor === null ? system : anchor.system + (elapsed - anchor.elapsed);
    if (ms < system || ms >= system + 2) {
      anchor = { elapsed, system };
      ms = system;
    }
    const seconds = Math.floor(ms / 1000);
    return {
      seconds: BigInt(seconds),
      nanoseconds: Math.floor((ms - seconds * 1000) * 1000000),
    };
  },
  resolution() {
    return {
      seconds: 0n,
      nanoseconds: 1000, // ~1us; browsers clamp this far coarser
    };
  },
};

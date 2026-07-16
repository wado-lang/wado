// Browser WASI P3 clocks shim.

export const monotonicClock = {
  now() {
    return BigInt(Math.round(performance.now() * 1e6));
  },
  resolution() {
    return 1n;
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
    return { seconds: 0n, nanoseconds: 1000000 };
  },
};

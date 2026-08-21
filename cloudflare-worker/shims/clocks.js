// wasi:clocks for a Worker. `Date.now()` advances only on I/O inside a request,
// so a duration measured across pure computation reads as zero.

export const monotonicClock = {
  now: () => BigInt(Math.round(Date.now() * 1e6)),
  getResolution: () => 1_000_000n,
  // A Worker has no sleep it can bill to a request, so a guest that waits or
  // backs off returns at once.
  waitUntil: async () => {},
  waitFor: async () => {},
};

export const systemClock = {
  // `now` answers an instant, `get-resolution` a duration.
  now: () => {
    const ms = Date.now();
    return { seconds: BigInt(Math.floor(ms / 1000)), nanoseconds: (ms % 1000) * 1_000_000 };
  },
  getResolution: () => 1_000_000n,
};

export const types = {};

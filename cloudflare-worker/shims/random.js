// wasi:random for a Worker. A `HashMap` asks `insecure-seed` for the seed it
// hashes under, which must be hard to guess, not cryptographic.

export const insecureSeed = {
  getInsecureSeed: () => {
    const words = crypto.getRandomValues(new BigUint64Array(2));
    return [words[0], words[1]];
  },
};

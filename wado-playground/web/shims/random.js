// Browser WASI P3 random shim (crypto.getRandomValues).

function fill(len) {
  const out = new Uint8Array(len);
  for (let off = 0; off < len; off += 65536) {
    crypto.getRandomValues(out.subarray(off, Math.min(off + 65536, len)));
  }
  return out;
}

function u64() {
  const b = fill(8);
  return new DataView(b.buffer).getBigUint64(0, true);
}

export const random = {
  getRandomBytes(len) { return fill(Number(len)); },
  getRandomU64() { return u64(); },
};

export const insecure = {
  getInsecureRandomBytes(len) { return fill(Number(len)); },
  getInsecureRandomU64() { return u64(); },
};

export const insecureSeed = {
  insecureSeed() { return [u64(), u64()]; },
};

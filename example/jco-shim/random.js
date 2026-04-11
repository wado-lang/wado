// Minimal WASI P3 random shim for jco-transpiled Wado programs
import { randomBytes } from "node:crypto";

export const random = {
  getRandomBytes(len) {
    return new Uint8Array(randomBytes(len));
  },
  getRandomU64() {
    const buf = randomBytes(8);
    return new DataView(buf.buffer).getBigUint64(0, true);
  },
};

export const insecure = {
  getInsecureRandomBytes(len) {
    return new Uint8Array(randomBytes(len));
  },
  getInsecureRandomU64() {
    const buf = randomBytes(8);
    return new DataView(buf.buffer).getBigUint64(0, true);
  },
};

export const insecureSeed = {
  insecureSeed() {
    const buf = randomBytes(16);
    return [
      new DataView(buf.buffer).getBigUint64(0, true),
      new DataView(buf.buffer).getBigUint64(8, true),
    ];
  },
};

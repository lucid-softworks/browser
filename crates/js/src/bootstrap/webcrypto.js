// Web Crypto `crypto.subtle`, layered onto the `crypto` object from browser-env. Pure-JS digests
// (SHA-1/256/384/512) and HMAC (sign/verify/generateKey/importKey/exportKey); values are processed
// over byte arrays and the spec-async methods return Promises. AES and asymmetric algorithms
// (RSA/ECDSA/ECDH) are out of scope; getRandomValues/randomUUID already exist on `crypto` (OS CSPRNG).
(function () {
  if (!globalThis.crypto || (globalThis.crypto.subtle && globalThis.crypto.subtle.digest)) { return; }
  function def(o, n, v) { Object.defineProperty(o, n, { value: v, enumerable: false, configurable: true, writable: true }); }
  function err(name, msg) { return new globalThis.DOMException(msg || name, name); }

  // ---- byte helpers ------------------------------------------------------------------------
  function toBytes(data) {
    if (data instanceof ArrayBuffer) { return Array.prototype.slice.call(new Uint8Array(data)); }
    if (data && data.buffer instanceof ArrayBuffer) { return Array.prototype.slice.call(new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength)); }
    throw new TypeError("argument must be a BufferSource");
  }
  function toBuffer(bytes) { return new Uint8Array(bytes).buffer; }

  // ---- SHA-1 / SHA-256 (32-bit) ------------------------------------------------------------
  function rotr(x, n) { return ((x >>> n) | (x << (32 - n))) >>> 0; }
  function rol(x, n) { return ((x << n) | (x >>> (32 - n))) >>> 0; }
  function pad64(bytes) {
    var msg = bytes.slice();
    msg.push(0x80);
    while (msg.length % 64 !== 56) { msg.push(0); }
    var bitLen = bytes.length * 8, hi = Math.floor(bitLen / 0x100000000) >>> 0, lo = bitLen >>> 0;
    msg.push((hi >>> 24) & 255, (hi >>> 16) & 255, (hi >>> 8) & 255, hi & 255, (lo >>> 24) & 255, (lo >>> 16) & 255, (lo >>> 8) & 255, lo & 255);
    return msg;
  }
  function be32(words) { var out = []; for (var i = 0; i < words.length; i++) { out.push((words[i] >>> 24) & 255, (words[i] >>> 16) & 255, (words[i] >>> 8) & 255, words[i] & 255); } return out; }

  var SHA256_K = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2];

  function sha256(bytes) {
    var h = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    var msg = pad64(bytes), w = new Array(64);
    for (var off = 0; off < msg.length; off += 64) {
      for (var t = 0; t < 16; t++) { w[t] = ((msg[off + t * 4] << 24) | (msg[off + t * 4 + 1] << 16) | (msg[off + t * 4 + 2] << 8) | msg[off + t * 4 + 3]) >>> 0; }
      for (t = 16; t < 64; t++) {
        var s0 = (rotr(w[t - 15], 7) ^ rotr(w[t - 15], 18) ^ (w[t - 15] >>> 3)) >>> 0;
        var s1 = (rotr(w[t - 2], 17) ^ rotr(w[t - 2], 19) ^ (w[t - 2] >>> 10)) >>> 0;
        w[t] = (w[t - 16] + s0 + w[t - 7] + s1) >>> 0;
      }
      var a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
      for (t = 0; t < 64; t++) {
        var S1 = (rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)) >>> 0;
        var ch = ((e & f) ^ ((~e) & g)) >>> 0;
        var t1 = (hh + S1 + ch + SHA256_K[t] + w[t]) >>> 0;
        var S0 = (rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)) >>> 0;
        var maj = ((a & b) ^ (a & c) ^ (b & c)) >>> 0;
        var t2 = (S0 + maj) >>> 0;
        hh = g; g = f; f = e; e = (d + t1) >>> 0; d = c; c = b; b = a; a = (t1 + t2) >>> 0;
      }
      h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0;
      h[4] = (h[4] + e) >>> 0; h[5] = (h[5] + f) >>> 0; h[6] = (h[6] + g) >>> 0; h[7] = (h[7] + hh) >>> 0;
    }
    return be32(h);
  }

  function sha1(bytes) {
    var h = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    var msg = pad64(bytes), w = new Array(80);
    for (var off = 0; off < msg.length; off += 64) {
      for (var t = 0; t < 16; t++) { w[t] = ((msg[off + t * 4] << 24) | (msg[off + t * 4 + 1] << 16) | (msg[off + t * 4 + 2] << 8) | msg[off + t * 4 + 3]) >>> 0; }
      for (t = 16; t < 80; t++) { w[t] = rol((w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]) >>> 0, 1); }
      var a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
      for (t = 0; t < 80; t++) {
        var f, k;
        if (t < 20) { f = ((b & c) | ((~b) & d)) >>> 0; k = 0x5a827999; }
        else if (t < 40) { f = (b ^ c ^ d) >>> 0; k = 0x6ed9eba1; }
        else if (t < 60) { f = ((b & c) | (b & d) | (c & d)) >>> 0; k = 0x8f1bbcdc; }
        else { f = (b ^ c ^ d) >>> 0; k = 0xca62c1d6; }
        var tmp = (rol(a, 5) + f + e + k + w[t]) >>> 0;
        e = d; d = c; c = rol(b, 30); b = a; a = tmp;
      }
      h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0; h[4] = (h[4] + e) >>> 0;
    }
    return be32(h);
  }

  // ---- SHA-512 / SHA-384 (64-bit via BigInt) -----------------------------------------------
  var MASK64 = (1n << 64n) - 1n;
  function rotr64(x, n) { return ((x >> n) | (x << (64n - n))) & MASK64; }
  var SHA512_K = [
    0x428a2f98d728ae22n, 0x7137449123ef65cdn, 0xb5c0fbcfec4d3b2fn, 0xe9b5dba58189dbbcn, 0x3956c25bf348b538n, 0x59f111f1b605d019n, 0x923f82a4af194f9bn, 0xab1c5ed5da6d8118n,
    0xd807aa98a3030242n, 0x12835b0145706fben, 0x243185be4ee4b28cn, 0x550c7dc3d5ffb4e2n, 0x72be5d74f27b896fn, 0x80deb1fe3b1696b1n, 0x9bdc06a725c71235n, 0xc19bf174cf692694n,
    0xe49b69c19ef14ad2n, 0xefbe4786384f25e3n, 0x0fc19dc68b8cd5b5n, 0x240ca1cc77ac9c65n, 0x2de92c6f592b0275n, 0x4a7484aa6ea6e483n, 0x5cb0a9dcbd41fbd4n, 0x76f988da831153b5n,
    0x983e5152ee66dfabn, 0xa831c66d2db43210n, 0xb00327c898fb213fn, 0xbf597fc7beef0ee4n, 0xc6e00bf33da88fc2n, 0xd5a79147930aa725n, 0x06ca6351e003826fn, 0x142929670a0e6e70n,
    0x27b70a8546d22ffcn, 0x2e1b21385c26c926n, 0x4d2c6dfc5ac42aedn, 0x53380d139d95b3dfn, 0x650a73548baf63den, 0x766a0abb3c77b2a8n, 0x81c2c92e47edaee6n, 0x92722c851482353bn,
    0xa2bfe8a14cf10364n, 0xa81a664bbc423001n, 0xc24b8b70d0f89791n, 0xc76c51a30654be30n, 0xd192e819d6ef5218n, 0xd69906245565a910n, 0xf40e35855771202an, 0x106aa07032bbd1b8n,
    0x19a4c116b8d2d0c8n, 0x1e376c085141ab53n, 0x2748774cdf8eeb99n, 0x34b0bcb5e19b48a8n, 0x391c0cb3c5c95a63n, 0x4ed8aa4ae3418acbn, 0x5b9cca4f7763e373n, 0x682e6ff3d6b2b8a3n,
    0x748f82ee5defb2fcn, 0x78a5636f43172f60n, 0x84c87814a1f0ab72n, 0x8cc702081a6439ecn, 0x90befffa23631e28n, 0xa4506cebde82bde9n, 0xbef9a3f7b2c67915n, 0xc67178f2e372532bn,
    0xca273eceea26619cn, 0xd186b8c721c0c207n, 0xeada7dd6cde0eb1en, 0xf57d4f7fee6ed178n, 0x06f067aa72176fban, 0x0a637dc5a2c898a6n, 0x113f9804bef90daen, 0x1b710b35131c471bn,
    0x28db77f523047d84n, 0x32caab7b40c72493n, 0x3c9ebe0a15c9bebcn, 0x431d67c49c100d4cn, 0x4cc5d4becb3e42b6n, 0x597f299cfc657e2an, 0x5fcb6fab3ad6faecn, 0x6c44198c4a475817n];

  function sha512core(bytes, h, outLen) {
    var msg = bytes.slice();
    msg.push(0x80);
    while (msg.length % 128 !== 112) { msg.push(0); }
    var bitLen = BigInt(bytes.length) * 8n;
    for (var i = 0; i < 8; i++) { msg.push(0); }                 // high 64 bits of the 128-bit length
    for (i = 7; i >= 0; i--) { msg.push(Number((bitLen >> BigInt(i * 8)) & 0xffn)); }
    h = h.slice();
    var w = new Array(80);
    for (var off = 0; off < msg.length; off += 128) {
      for (var t = 0; t < 16; t++) { var v = 0n; for (var b = 0; b < 8; b++) { v = (v << 8n) | BigInt(msg[off + t * 8 + b]); } w[t] = v; }
      for (t = 16; t < 80; t++) {
        var s0 = rotr64(w[t - 15], 1n) ^ rotr64(w[t - 15], 8n) ^ (w[t - 15] >> 7n);
        var s1 = rotr64(w[t - 2], 19n) ^ rotr64(w[t - 2], 61n) ^ (w[t - 2] >> 6n);
        w[t] = (w[t - 16] + s0 + w[t - 7] + s1) & MASK64;
      }
      var a = h[0], bb = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
      for (t = 0; t < 80; t++) {
        var S1 = rotr64(e, 14n) ^ rotr64(e, 18n) ^ rotr64(e, 41n);
        var ch = (e & f) ^ ((~e & MASK64) & g);
        var t1 = (hh + S1 + ch + SHA512_K[t] + w[t]) & MASK64;
        var S0 = rotr64(a, 28n) ^ rotr64(a, 34n) ^ rotr64(a, 39n);
        var maj = (a & bb) ^ (a & c) ^ (bb & c);
        var t2 = (S0 + maj) & MASK64;
        hh = g; g = f; f = e; e = (d + t1) & MASK64; d = c; c = bb; bb = a; a = (t1 + t2) & MASK64;
      }
      h[0] = (h[0] + a) & MASK64; h[1] = (h[1] + bb) & MASK64; h[2] = (h[2] + c) & MASK64; h[3] = (h[3] + d) & MASK64;
      h[4] = (h[4] + e) & MASK64; h[5] = (h[5] + f) & MASK64; h[6] = (h[6] + g) & MASK64; h[7] = (h[7] + hh) & MASK64;
    }
    var out = [];
    for (i = 0; i < 8; i++) { for (b = 7; b >= 0; b--) { out.push(Number((h[i] >> BigInt(b * 8)) & 0xffn)); } }
    return out.slice(0, outLen);
  }
  function sha512(bytes) { return sha512core(bytes, [0x6a09e667f3bcc908n, 0xbb67ae8584caa73bn, 0x3c6ef372fe94f82bn, 0xa54ff53a5f1d36f1n, 0x510e527fade682d1n, 0x9b05688c2b3e6c1fn, 0x1f83d9abfb41bd6bn, 0x5be0cd19137e2179n], 64); }
  function sha384(bytes) { return sha512core(bytes, [0xcbbb9d5dc1059ed8n, 0x629a292a367cd507n, 0x9159015a3070dd17n, 0x152fecd8f70e5939n, 0x67332667ffc00b31n, 0x8eb44a8768581511n, 0xdb0c2e0d64f98fa7n, 0x47b5481dbefa4fa4n], 48); }

  // ---- digest dispatch ---------------------------------------------------------------------
  function normHash(alg) {
    var name = (typeof alg === "string" ? alg : (alg && (alg.name || (alg.hash && (alg.hash.name || alg.hash))))) || "";
    return String(name).toUpperCase();
  }
  function digestBytes(name, bytes) {
    if (name === "SHA-1") { return sha1(bytes); }
    if (name === "SHA-256") { return sha256(bytes); }
    if (name === "SHA-384") { return sha384(bytes); }
    if (name === "SHA-512") { return sha512(bytes); }
    return null;
  }
  function blockSize(name) { return (name === "SHA-384" || name === "SHA-512") ? 128 : 64; }
  function hmac(hashName, keyBytes, msgBytes) {
    var block = blockSize(hashName), key = keyBytes.slice();
    if (key.length > block) { key = digestBytes(hashName, key); }
    while (key.length < block) { key.push(0); }
    var ipad = [], opad = [];
    for (var i = 0; i < block; i++) { ipad.push(key[i] ^ 0x36); opad.push(key[i] ^ 0x5c); }
    return digestBytes(hashName, opad.concat(digestBytes(hashName, ipad.concat(msgBytes))));
  }

  // ---- CryptoKey ---------------------------------------------------------------------------
  function CryptoKey() {}
  function makeKey(type, extractable, algorithm, usages, bytes, hashName) {
    var k = Object.create(CryptoKey.prototype);
    k.type = type; k.extractable = !!extractable; k.algorithm = algorithm; k.usages = usages.slice();
    Object.defineProperty(k, "__bytes", { value: bytes, enumerable: false });
    Object.defineProperty(k, "__hash", { value: hashName, enumerable: false });
    return k;
  }

  // ---- SubtleCrypto ------------------------------------------------------------------------
  function SubtleCrypto() {}
  var subtle = Object.create(SubtleCrypto.prototype);
  def(subtle, "digest", function (algorithm, data) {
    return new Promise(function (resolve, reject) {
      try {
        var name = normHash(algorithm), bytes = toBytes(data), out = digestBytes(name, bytes);
        if (!out) { reject(err("NotSupportedError", "unsupported digest algorithm: " + name)); return; }
        resolve(toBuffer(out));
      } catch (e) { reject(e); }
    });
  });
  def(subtle, "importKey", function (format, keyData, algorithm, extractable, usages) {
    return new Promise(function (resolve, reject) {
      try {
        if (format !== "raw") { reject(err("NotSupportedError", "only 'raw' key import is supported")); return; }
        var algName = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
        if (algName !== "HMAC") { reject(err("NotSupportedError", "only HMAC keys are supported")); return; }
        var hashName = normHash(algorithm.hash || algorithm);
        if (!digestBytes(hashName, [])) { reject(err("NotSupportedError", "unsupported hash: " + hashName)); return; }
        var bytes = toBytes(keyData);
        resolve(makeKey("secret", extractable, { name: "HMAC", hash: { name: hashName }, length: bytes.length * 8 }, usages || [], bytes, hashName));
      } catch (e) { reject(e); }
    });
  });
  def(subtle, "exportKey", function (format, key) {
    return new Promise(function (resolve, reject) {
      try {
        if (format !== "raw") { reject(err("NotSupportedError", "only 'raw' key export is supported")); return; }
        if (!(key instanceof CryptoKey)) { reject(new TypeError("not a CryptoKey")); return; }
        if (!key.extractable) { reject(err("InvalidAccessError", "key is not extractable")); return; }
        resolve(toBuffer(key.__bytes));
      } catch (e) { reject(e); }
    });
  });
  def(subtle, "generateKey", function (algorithm, extractable, usages) {
    return new Promise(function (resolve, reject) {
      try {
        var algName = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
        if (algName !== "HMAC") { reject(err("NotSupportedError", "only HMAC key generation is supported")); return; }
        var hashName = normHash(algorithm.hash || algorithm);
        var bits = (algorithm && algorithm.length) ? algorithm.length : blockSize(hashName) * 8;
        var arr = new Uint8Array(Math.ceil(bits / 8));
        globalThis.crypto.getRandomValues(arr);
        resolve(makeKey("secret", extractable, { name: "HMAC", hash: { name: hashName }, length: bits }, usages || [], Array.prototype.slice.call(arr), hashName));
      } catch (e) { reject(e); }
    });
  });
  def(subtle, "sign", function (algorithm, key, data) {
    return new Promise(function (resolve, reject) {
      try {
        var algName = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
        if (algName !== "HMAC" || !(key instanceof CryptoKey)) { reject(err("NotSupportedError", "only HMAC signing is supported")); return; }
        resolve(toBuffer(hmac(key.__hash, key.__bytes, toBytes(data))));
      } catch (e) { reject(e); }
    });
  });
  def(subtle, "verify", function (algorithm, key, signature, data) {
    return new Promise(function (resolve, reject) {
      try {
        var algName = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
        if (algName !== "HMAC" || !(key instanceof CryptoKey)) { reject(err("NotSupportedError", "only HMAC verification is supported")); return; }
        var expected = hmac(key.__hash, key.__bytes, toBytes(data)), got = toBytes(signature);
        if (expected.length !== got.length) { resolve(false); return; }
        var diff = 0;
        for (var i = 0; i < expected.length; i++) { diff |= expected[i] ^ got[i]; }   // constant-time compare
        resolve(diff === 0);
      } catch (e) { reject(e); }
    });
  });

  globalThis.crypto.subtle = subtle;
  def(globalThis, "SubtleCrypto", SubtleCrypto);
  def(globalThis, "CryptoKey", CryptoKey);
  if (typeof globalThis.Crypto !== "function") { def(globalThis, "Crypto", function () {}); }
})();

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

  // ---- AES block cipher + CBC/CTR modes ----------------------------------------------------
  var AES_SBOX = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16];
  var AES_INV_SBOX = (function () { var inv = new Array(256); for (var i = 0; i < 256; i++) { inv[AES_SBOX[i]] = i; } return inv; })();
  function xtime(x) { return ((x << 1) ^ ((x & 0x80) ? 0x1b : 0)) & 0xff; }
  function gmul(a, b) { var p = 0; for (var i = 0; i < 8; i++) { if (b & 1) { p ^= a; } var hi = a & 0x80; a = (a << 1) & 0xff; if (hi) { a ^= 0x1b; } b >>= 1; } return p & 0xff; }
  function aesExpandKey(key) {
    var Nk = key.length / 4, Nr = Nk + 6, total = 4 * (Nr + 1), w = new Array(total * 4), i, rcon = 1;
    for (i = 0; i < key.length; i++) { w[i] = key[i]; }
    for (i = Nk; i < total; i++) {
      var t = [w[(i - 1) * 4], w[(i - 1) * 4 + 1], w[(i - 1) * 4 + 2], w[(i - 1) * 4 + 3]];
      if (i % Nk === 0) {
        var tmp = t[0]; t[0] = AES_SBOX[t[1]] ^ rcon; t[1] = AES_SBOX[t[2]]; t[2] = AES_SBOX[t[3]]; t[3] = AES_SBOX[tmp];
        rcon = xtime(rcon);
      } else if (Nk > 6 && i % Nk === 4) {
        t = [AES_SBOX[t[0]], AES_SBOX[t[1]], AES_SBOX[t[2]], AES_SBOX[t[3]]];
      }
      for (var j = 0; j < 4; j++) { w[i * 4 + j] = w[(i - Nk) * 4 + j] ^ t[j]; }
    }
    return { w: w, Nr: Nr };
  }
  function addRK(s, w, round) { for (var i = 0; i < 16; i++) { s[i] ^= w[round * 16 + i]; } }
  function subB(s) { for (var i = 0; i < 16; i++) { s[i] = AES_SBOX[s[i]]; } }
  function invSubB(s) { for (var i = 0; i < 16; i++) { s[i] = AES_INV_SBOX[s[i]]; } }
  // State is column-major (s[row + 4*col]); ShiftRows rotates row r left by r bytes.
  function shiftR(s) { var t; t = s[1]; s[1] = s[5]; s[5] = s[9]; s[9] = s[13]; s[13] = t; t = s[2]; s[2] = s[10]; s[10] = t; t = s[6]; s[6] = s[14]; s[14] = t; t = s[15]; s[15] = s[11]; s[11] = s[7]; s[7] = s[3]; s[3] = t; }
  function invShiftR(s) { var t; t = s[13]; s[13] = s[9]; s[9] = s[5]; s[5] = s[1]; s[1] = t; t = s[2]; s[2] = s[10]; s[10] = t; t = s[6]; s[6] = s[14]; s[14] = t; t = s[3]; s[3] = s[7]; s[7] = s[11]; s[11] = s[15]; s[15] = t; }
  function mixC(s) { for (var c = 0; c < 4; c++) { var i = c * 4, a0 = s[i], a1 = s[i + 1], a2 = s[i + 2], a3 = s[i + 3]; s[i] = gmul(a0, 2) ^ gmul(a1, 3) ^ a2 ^ a3; s[i + 1] = a0 ^ gmul(a1, 2) ^ gmul(a2, 3) ^ a3; s[i + 2] = a0 ^ a1 ^ gmul(a2, 2) ^ gmul(a3, 3); s[i + 3] = gmul(a0, 3) ^ a1 ^ a2 ^ gmul(a3, 2); } }
  function invMixC(s) { for (var c = 0; c < 4; c++) { var i = c * 4, a0 = s[i], a1 = s[i + 1], a2 = s[i + 2], a3 = s[i + 3]; s[i] = gmul(a0, 14) ^ gmul(a1, 11) ^ gmul(a2, 13) ^ gmul(a3, 9); s[i + 1] = gmul(a0, 9) ^ gmul(a1, 14) ^ gmul(a2, 11) ^ gmul(a3, 13); s[i + 2] = gmul(a0, 13) ^ gmul(a1, 9) ^ gmul(a2, 14) ^ gmul(a3, 11); s[i + 3] = gmul(a0, 11) ^ gmul(a1, 13) ^ gmul(a2, 9) ^ gmul(a3, 14); } }
  function aesEncBlock(inp, ks) { var s = inp.slice(0, 16); addRK(s, ks.w, 0); for (var r = 1; r < ks.Nr; r++) { subB(s); shiftR(s); mixC(s); addRK(s, ks.w, r); } subB(s); shiftR(s); addRK(s, ks.w, ks.Nr); return s; }
  function aesDecBlock(inp, ks) { var s = inp.slice(0, 16); addRK(s, ks.w, ks.Nr); for (var r = ks.Nr - 1; r >= 1; r--) { invShiftR(s); invSubB(s); addRK(s, ks.w, r); invMixC(s); } invShiftR(s); invSubB(s); addRK(s, ks.w, 0); return s; }
  function pkcs7Pad(bytes) { var pad = 16 - (bytes.length % 16), out = bytes.slice(); for (var i = 0; i < pad; i++) { out.push(pad); } return out; }
  function pkcs7Unpad(bytes) {
    if (!bytes.length || bytes.length % 16 !== 0) { throw err("OperationError", "invalid padding"); }
    var pad = bytes[bytes.length - 1];
    if (pad < 1 || pad > 16 || pad > bytes.length) { throw err("OperationError", "invalid padding"); }
    for (var i = bytes.length - pad; i < bytes.length; i++) { if (bytes[i] !== pad) { throw err("OperationError", "invalid padding"); } }
    return bytes.slice(0, bytes.length - pad);
  }
  function aesCbc(ks, iv, data, encrypt) {
    if (encrypt) {
      var padded = pkcs7Pad(data), out = [], prev = iv.slice(0, 16);
      for (var off = 0; off < padded.length; off += 16) {
        var blk = padded.slice(off, off + 16);
        for (var i = 0; i < 16; i++) { blk[i] ^= prev[i]; }
        var enc = aesEncBlock(blk, ks);
        for (i = 0; i < 16; i++) { out.push(enc[i]); }
        prev = enc;
      }
      return out;
    }
    if (data.length % 16 !== 0) { throw err("OperationError", "ciphertext length not a multiple of the block size"); }
    var dout = [], dprev = iv.slice(0, 16);
    for (var d = 0; d < data.length; d += 16) {
      var cblk = data.slice(d, d + 16), dec = aesDecBlock(cblk, ks);
      for (var k = 0; k < 16; k++) { dec[k] ^= dprev[k]; dout.push(dec[k]); }
      dprev = cblk;
    }
    return pkcs7Unpad(dout);
  }
  function incCtr(c) { for (var i = 15; i >= 0; i--) { c[i] = (c[i] + 1) & 0xff; if (c[i] !== 0) { break; } } }
  function aesCtr(ks, counter, data) {   // CTR is symmetric — same routine encrypts and decrypts
    var out = [], ctr = counter.slice(0, 16);
    for (var off = 0; off < data.length; off += 16) {
      var keystream = aesEncBlock(ctr, ks), n = Math.min(16, data.length - off);
      for (var i = 0; i < n; i++) { out.push(data[off + i] ^ keystream[i]); }
      incCtr(ctr);
    }
    return out;
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
  // SubtleCrypto.supports(operation, algorithm) — feature-detection (newer spec addition). Report
  // true only for the operation/algorithm pairs we actually implement, so callers neither hit a
  // TypeError nor skip our working algorithms.
  function supports(operation, algorithm) {
    var op = String(operation || "").toLowerCase();
    var name = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
    if (op === "digest") { return !!digestBytes(name, []); }
    if (op === "sign" || op === "verify") { return name === "HMAC"; }
    if (op === "encrypt" || op === "decrypt") { return name === "AES-CBC" || name === "AES-CTR"; }
    if (op === "importkey" || op === "exportkey" || op === "generatekey") { return name === "HMAC" || name === "AES-CBC" || name === "AES-CTR"; }
    return false;
  }
  def(SubtleCrypto, "supports", supports);
  def(subtle, "supports", supports);
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
        var bytes = toBytes(keyData);
        if (algName === "AES-CBC" || algName === "AES-CTR") {
          if ([16, 24, 32].indexOf(bytes.length) < 0) { reject(err("DataError", "invalid AES key length")); return; }
          resolve(makeKey("secret", extractable, { name: algName, length: bytes.length * 8 }, usages || [], bytes, null));
          return;
        }
        if (algName !== "HMAC") { reject(err("NotSupportedError", "unsupported key import algorithm: " + algName)); return; }
        var hashName = normHash(algorithm.hash || algorithm);
        if (!digestBytes(hashName, [])) { reject(err("NotSupportedError", "unsupported hash: " + hashName)); return; }
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
        if (algName === "AES-CBC" || algName === "AES-CTR") {
          var aesBits = (algorithm && algorithm.length) ? algorithm.length : 256;
          if ([128, 192, 256].indexOf(aesBits) < 0) { reject(err("OperationError", "invalid AES key length")); return; }
          var aesArr = new Uint8Array(aesBits / 8);
          globalThis.crypto.getRandomValues(aesArr);
          resolve(makeKey("secret", extractable, { name: algName, length: aesBits }, usages || [], Array.prototype.slice.call(aesArr), null));
          return;
        }
        if (algName !== "HMAC") { reject(err("NotSupportedError", "unsupported key generation algorithm: " + algName)); return; }
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

  function aesCipher(algorithm, key, data, encrypt) {
    var algName = (algorithm && (algorithm.name || algorithm) || "").toUpperCase();
    if (!(key instanceof CryptoKey) || (algName !== "AES-CBC" && algName !== "AES-CTR")) { throw err("NotSupportedError", "unsupported cipher algorithm: " + algName); }
    var ks = aesExpandKey(key.__bytes), bytes = toBytes(data);
    if (algName === "AES-CBC") { return aesCbc(ks, toBytes(algorithm.iv), bytes, encrypt); }
    return aesCtr(ks, toBytes(algorithm.counter), bytes);   // CTR: same op encrypts and decrypts
  }
  def(subtle, "encrypt", function (algorithm, key, data) { return new Promise(function (resolve, reject) { try { resolve(toBuffer(aesCipher(algorithm, key, data, true))); } catch (e) { reject(e); } }); });
  def(subtle, "decrypt", function (algorithm, key, data) { return new Promise(function (resolve, reject) { try { resolve(toBuffer(aesCipher(algorithm, key, data, false))); } catch (e) { reject(e); } }); });

  globalThis.crypto.subtle = subtle;
  def(globalThis, "SubtleCrypto", SubtleCrypto);
  def(globalThis, "CryptoKey", CryptoKey);
  if (typeof globalThis.Crypto !== "function") { def(globalThis, "Crypto", function () {}); }
})();

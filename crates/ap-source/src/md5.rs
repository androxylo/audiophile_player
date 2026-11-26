// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Zero-dependency clean-room RFC 1321 MD5 & RFC 9639 §8.2 streaming FLAC sample MD5 verifier.
//!
//! Used by Ultimate Mode learn-on-play (`U5b` / `U14`, `docs/research/07-ultimate-mode.md` §5.2):
//! computes the exact MD5 signature over decoded signed little-endian interleaved integer samples
//! (sign-extended to whole bytes per RFC 9639 §8.2) during playback and only allows writing
//! whole-track YAML results when the computed digest matches `STREAMINFO.md5`.

/// Incremental RFC 1321 MD5 hasher (safe Rust, zero external dependencies).
#[derive(Debug, Clone)]
pub struct Md5Hasher {
    state: [u32; 4],
    total_bytes: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Default for Md5Hasher {
    fn default() -> Self {
        Self::new()
    }
}

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
    0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
    0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
    0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
    0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
    0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
    0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
    0xeb86_d391,
];

impl Md5Hasher {
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            total_bytes: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);

        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }

        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }

        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let total_bits = self.total_bytes.wrapping_mul(8);
        self.update(&[0x80]);
        let pad_zeros = if self.buf_len <= 56 {
            56 - self.buf_len
        } else {
            120 - self.buf_len
        };
        let zeros = [0u8; 64];
        let mut rem = pad_zeros;
        while rem > 0 {
            let step = rem.min(64);
            self.update(&zeros[..step]);
            rem -= step;
        }
        self.update(&total_bits.to_le_bytes());
        debug_assert_eq!(self.buf_len, 0);

        let mut out = [0u8; 16];
        for (i, &w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    #[inline]
    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }

        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) & 15),
                32..=47 => (b ^ c ^ d, (3 * i + 5) & 15),
                _ => (c ^ (b | (!d)), (7 * i) & 15),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(MD5_S[i]));
            a = temp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

/// Format a 16-byte MD5 digest as 32 lowercase hex characters.
pub fn md5_to_hex(digest: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Streaming RFC 9639 §8.2 FLAC unencoded audio MD5 hasher.
///
/// Feeds raw decoded signed integer samples (at original source channel count, BEFORE mono->stereo
/// duplication!) encoded as signed little-endian integers sign-extended to `ceil(bits/8)` bytes.
#[derive(Debug, Clone)]
pub struct FlacSampleMd5Hasher {
    hasher: Md5Hasher,
    bytes_per_sample: usize,
    scratch: Vec<u8>,
}

impl FlacSampleMd5Hasher {
    pub fn new(bits_per_sample: u16) -> Self {
        let bps = usize::from(bits_per_sample.clamp(1, 32)).div_ceil(8);
        Self {
            hasher: Md5Hasher::new(),
            bytes_per_sample: bps,
            scratch: Vec::with_capacity(4096),
        }
    }

    /// Feed interleaved raw decoded signed samples (unshifted, sign-extended in `i32`).
    pub fn update_raw_samples(&mut self, interleaved: &[i32]) {
        let bps = self.bytes_per_sample;
        self.scratch.clear();
        self.scratch.reserve(interleaved.len().saturating_mul(bps));
        match bps {
            1 => {
                for &s in interleaved {
                    self.scratch.push(s as u8);
                }
            }
            2 => {
                for &s in interleaved {
                    let le = (s as i16).to_le_bytes();
                    self.scratch.extend_from_slice(&le);
                }
            }
            3 => {
                for &s in interleaved {
                    let le = s.to_le_bytes();
                    self.scratch.extend_from_slice(&le[0..3]);
                }
            }
            _ => {
                for &s in interleaved {
                    self.scratch.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
        self.hasher.update(&self.scratch);
    }

    /// Finish hashing and return the 16-byte RFC 9639 `STREAMINFO.md5` digest.
    pub fn finalize(self) -> [u8; 16] {
        self.hasher.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1321_official_test_vectors_match_bit_exact() {
        let vectors = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in vectors {
            let mut h = Md5Hasher::new();
            // Exercise chunked streaming updates (odd byte splits)
            for chunk in input.as_bytes().chunks(7) {
                h.update(chunk);
            }
            assert_eq!(md5_to_hex(&h.finalize()), expected, "failed for '{input}'");
        }
    }
}

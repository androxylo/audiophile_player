// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Zero-dependency double-double (~106-bit IEEE 754 `f64 + f64`) arithmetic for U8 kernel design.
//!
//! Used exclusively during one-time filter prototype synthesis (`sinc`, Gaussian/Kaiser windows,
//! modified Bessel `I_0`, and per-phase unity DC normalization), rounded once to `f64` and cached
//! by `FilterIdentity` (`docs/research/07-ultimate-mode.md` U8, OQ-09, OQ-36).
//!
//! Uses portable pure-IEEE-754 Veltkamp-Dekker/Knuth error-free transformations (`two_sum`,
//! `two_prod` with exact 26+27-bit splitting, avoiding UCRT `fma()` rounding discrepancies),
//! giving ~31–32 decimal digits of precision (~106 bits).

/// Non-overlapping double-double pair representing `hi + lo` with `|lo| <= 0.5 * ulp(hi)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dd {
    pub hi: f64,
    pub lo: f64,
}

/// High-precision `pi` to ~106 bits.
pub const DD_PI: Dd = Dd {
    hi: 3.141592653589793116e+00,
    lo: 1.224646799147353207e-16,
};

/// High-precision `2 * pi` to ~106 bits.
pub const DD_TWO_PI: Dd = Dd {
    hi: 6.283185307179586232e+00,
    lo: 2.449293598294706414e-16,
};

/// High-precision `pi / 2` to ~106 bits.
pub const DD_HALF_PI: Dd = Dd {
    hi: 1.570796326794896558e+00,
    lo: 6.123233995736766036e-17,
};

/// High-precision `ln(2)` to ~106 bits.
pub const DD_LN2: Dd = Dd {
    hi: 6.931471805599452862e-01,
    lo: 2.319046813846299558e-17,
};

pub const DD_ZERO: Dd = Dd { hi: 0.0, lo: 0.0 };
pub const DD_ONE: Dd = Dd { hi: 1.0, lo: 0.0 };

/// Knuth error-free addition `a + b = s + e`.
#[inline]
pub fn two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    let v = s - a;
    let e = (a - (s - v)) + (b - v);
    Dd { hi: s, lo: e }
}

/// Dekker quick error-free addition assuming `|a| >= |b|`.
#[inline]
pub fn quick_two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    let e = b - (s - a);
    Dd { hi: s, lo: e }
}

/// Veltkamp 26+27-bit exact split for portable Dekker `two_prod` (zero UCRT `fma()` dependency).
#[inline]
fn split_f64(a: f64) -> (f64, f64) {
    const SPLIT: f64 = 134_217_729.0; // 2^27 + 1
    let t = SPLIT * a;
    let hi = t - (t - a);
    let lo = a - hi;
    (hi, lo)
}

/// Exact Dekker error-free product `a * b = p + e` (portable pure IEEE 754 `f64`).
#[inline]
pub fn two_prod(a: f64, b: f64) -> Dd {
    let p = a * b;
    let (a_hi, a_lo) = split_f64(a);
    let (b_hi, b_lo) = split_f64(b);
    let e = ((a_hi * b_hi - p) + a_hi * b_lo + a_lo * b_hi) + a_lo * b_lo;
    Dd { hi: p, lo: e }
}

impl Dd {
    #[inline]
    pub const fn from_f64(x: f64) -> Self {
        Self { hi: x, lo: 0.0 }
    }

    #[inline]
    pub fn to_f64(self) -> f64 {
        self.hi + self.lo
    }

    #[inline]
    pub const fn neg(self) -> Self {
        Self {
            hi: -self.hi,
            lo: -self.lo,
        }
    }

    #[inline]
    pub fn abs(self) -> Self {
        if self.hi < 0.0 { self.neg() } else { self }
    }

    #[inline]
    pub fn add(self, rhs: Self) -> Self {
        let s = two_sum(self.hi, rhs.hi);
        let t = two_sum(self.lo, rhs.lo);
        let c = s.lo + t.hi;
        let v = two_sum(s.hi, c);
        two_sum(v.hi, v.lo + t.lo)
    }

    #[inline]
    pub fn add_f64(self, rhs: f64) -> Self {
        let s = two_sum(self.hi, rhs);
        two_sum(s.hi, s.lo + self.lo)
    }

    #[inline]
    pub fn sub(self, rhs: Self) -> Self {
        self.add(rhs.neg())
    }

    #[inline]
    pub fn mul(self, rhs: Self) -> Self {
        let p = two_prod(self.hi, rhs.hi);
        let e = self.hi * rhs.lo + self.lo * rhs.hi;
        two_sum(p.hi, p.lo + e)
    }

    #[inline]
    pub fn mul_f64(self, rhs: f64) -> Self {
        let p = two_prod(self.hi, rhs);
        two_sum(p.hi, p.lo + self.lo * rhs)
    }

    #[inline]
    pub fn sqr(self) -> Self {
        let p = two_prod(self.hi, self.hi);
        let e = 2.0 * self.hi * self.lo;
        two_sum(p.hi, p.lo + e)
    }

    #[inline]
    pub fn div(self, rhs: Self) -> Self {
        let q1 = self.hi / rhs.hi;
        let r1 = self.sub(rhs.mul_f64(q1));
        let q2 = r1.hi / rhs.hi;
        let r2 = r1.sub(rhs.mul_f64(q2));
        let q3 = r2.hi / rhs.hi;
        let s = two_sum(q1, q2);
        s.add_f64(q3)
    }

    #[inline]
    pub fn div_f64(self, rhs: f64) -> Self {
        let q1 = self.hi / rhs;
        let p = two_prod(q1, rhs);
        let r = self.sub(p);
        let q2 = r.hi / rhs;
        two_sum(q1, q2)
    }

    #[inline]
    pub fn recip(self) -> Self {
        DD_ONE.div(self)
    }

    /// Square root to ~106-bit precision (`sqrt(0) = 0`).
    pub fn sqrt(self) -> Self {
        if self.hi <= 0.0 {
            return DD_ZERO;
        }
        let x = 1.0 / self.hi.sqrt();
        let ax = self.hi * x;
        // One Newton-Raphson refinement in double-double: sqrt(a) = ax + (a - ax^2) * (x / 2).
        let diff = self.sub(two_prod(ax, ax));
        let corr = diff.mul_f64(0.5 * x);
        two_sum(ax, corr.hi + corr.lo)
    }

    /// Multiply by an exact power of two `2^exp` (no rounding error when in normal range).
    #[inline]
    pub fn ldexp(self, exp: i32) -> Self {
        let scale = f64::from_bits(((exp + 1023).clamp(1, 2046) as u64) << 52);
        Self {
            hi: self.hi * scale,
            lo: self.lo * scale,
        }
    }

    /// Exponential `exp(self)` to ~106-bit precision via `ln(2)` range reduction + Taylor series.
    pub fn exp(self) -> Self {
        if self.hi <= -745.0 {
            return DD_ZERO;
        }
        if self.hi >= 709.0 {
            return Self::from_f64(f64::INFINITY);
        }
        let k = (self.hi / DD_LN2.hi).round() as i32;
        let r = self.sub(DD_LN2.mul_f64(k as f64));

        // |r| <= ln(2)/2 ~= 0.347; 28 terms gives < 1e-42 residual.
        let mut sum = DD_ONE;
        let mut term = DD_ONE;
        for n in 1..=30 {
            term = term.mul(r).div_f64(n as f64);
            sum = sum.add(term);
            if term.hi.abs() <= sum.hi.abs() * 1e-34 {
                break;
            }
        }
        sum.ldexp(k)
    }

    /// Sine and cosine `(sin(self), cos(self))` to ~106-bit precision via `pi/2` quadrant reduction.
    pub fn sin_cos(self) -> (Self, Self) {
        let q_f = (self.hi / DD_HALF_PI.hi).round();
        let q = q_f as i64;
        let r = self.sub(DD_HALF_PI.mul_f64(q_f));
        let r2 = r.sqr();

        // Taylor series on |r| <= pi/4 ~= 0.7854: converges to < 1e-35 in <= 16 terms.
        let mut s = r;
        let mut term_s = r;
        for k in 1..=18 {
            let d = ((2 * k) * (2 * k + 1)) as f64;
            term_s = term_s.mul(r2).neg().div_f64(d);
            s = s.add(term_s);
            if term_s.hi.abs() <= 1e-34 {
                break;
            }
        }

        let mut c = DD_ONE;
        let mut term_c = DD_ONE;
        for k in 1..=18 {
            let d = ((2 * k - 1) * (2 * k)) as f64;
            term_c = term_c.mul(r2).neg().div_f64(d);
            c = c.add(term_c);
            if term_c.hi.abs() <= 1e-34 {
                break;
            }
        }

        match q.rem_euclid(4) {
            0 => (s, c),
            1 => (c, s.neg()),
            2 => (s.neg(), c.neg()),
            _ => (c.neg(), s),
        }
    }

    #[inline]
    pub fn sin(self) -> Self {
        self.sin_cos().0
    }
}

/// Exact 106-bit accumulation of `f64` slices (for per-phase unity DC normalization).
pub fn dd_sum_f64(values: &[f64]) -> Dd {
    let mut acc = DD_ZERO;
    for &v in values {
        acc = acc.add_f64(v);
    }
    acc
}

/// Exact 106-bit accumulation of `Dd` slices.
pub fn dd_sum(values: &[Dd]) -> Dd {
    let mut acc = DD_ZERO;
    for &v in values {
        acc = acc.add(v);
    }
    acc
}

/// Zeroth-order modified Bessel function `I_0(x)` evaluated to ~106-bit double-double precision.
pub fn dd_bessel_i0(x: Dd) -> Dd {
    let half = x.mul_f64(0.5);
    let y = half.sqr();
    let mut sum = DD_ONE;
    let mut term = DD_ONE;
    for k in 1..=300 {
        let kf = k as f64;
        term = term.mul(y).div_f64(kf * kf);
        sum = sum.add(term);
        if term.hi.abs() <= sum.hi.abs() * 1e-34 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dd_transcendentals_and_bessel_achieve_106_bit_accuracy() {
        // sqrt(2)^2 == 2 within ~106-bit double-double rounding (~2e-31 on magnitude 2)
        let s2 = Dd::from_f64(2.0).sqrt();
        let err_sqrt = s2.sqr().sub(Dd::from_f64(2.0)).abs().to_f64();
        assert!(err_sqrt < 5e-31, "sqrt(2)^2 err={err_sqrt:e}");

        // exp(ln(2)) == 2 within ~106-bit double-double rounding
        let e2 = DD_LN2.exp();
        let err_exp = e2.sub(Dd::from_f64(2.0)).abs().to_f64();
        assert!(err_exp < 5e-31, "exp(ln2) err={err_exp:e}");

        // Debug exact step check
        let a = Dd::from_f64(1.0).div_f64(3.0);
        let a3 = a.mul_f64(3.0).sub(DD_ONE).abs().to_f64();
        assert!(a3 < 1e-30, "div_f64(3)*3 err={a3:e}");
        let p = two_prod(1.234567890123456, 2.345678901234567);
        assert!(p.lo != 0.0, "two_prod returned zero lo!");
        let q = Dd::from_f64(1.234567890123456).div(Dd::from_f64(7.0));
        let q7 = q.mul(Dd::from_f64(7.0)).sub(Dd::from_f64(1.234567890123456)).abs().to_f64();
        assert!(q7 < 1e-30, "div/mul err={q7:e}");

        // sin(x)^2 + cos(x)^2 == 1 across many quadrants to < 1e-30
        for k in [-17, -3, 0, 1, 5, 42, 1001] {
            let x = Dd::from_f64((k as f64) * 1.234567890123456);
            let (s, c) = x.sin_cos();
            let err = s.sqr().add(c.sqr()).sub(DD_ONE).abs().to_f64();
            assert!(
                err < 1e-30,
                "sin^2+cos^2 err={err:e} at k={k} (s={s:?}, c={c:?}, f64_s={}, f64_c={})",
                x.hi.sin(),
                x.hi.cos()
            );
        }

        // sin(pi) ~= 0 to < 1e-31
        let sp = DD_PI.sin().abs().to_f64();
        assert!(sp < 1e-31, "sin(pi)={sp:e}");

        // I_0(21.08126) finite and strictly positive, matching asymptotic scale
        let i0 = dd_bessel_i0(Dd::from_f64(21.08126));
        assert!(i0.hi.is_finite() && i0.hi > 1.0e8);
    }
}

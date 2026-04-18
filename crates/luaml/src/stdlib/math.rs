//! `math` stdlib extensions.
//!
//! Unlike other stdlib modules that claim a bare namespace of their own, this
//! module EXTENDS Lua's built-in `math` global. We fetch the existing `math`
//! table from globals, merge our additional functions into it, and return the
//! same table reference. When `install_all` then calls `globals.set("math",
//! table)`, it re-binds the name to the same object — constants like `math.pi`
//! and functions like `math.sqrt` remain intact.
//!
//! Added surface:
//! - Statistics: `mean`, `median`, `stddev`, `variance`, `percentile`
//! - Interpolation: `lerp`, `smoothstep`, `clamp`
//! - Combinatorics: `factorial`, `binom`
//!
//! All functions are synchronous and operate on `f64` (mlua auto-coerces Lua
//! integers). The tokio runtime handle is unused here; kept in the signature
//! to satisfy the [`LuamlStdlibModule`] trait.

use mlua::{Lua, Table};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Extension module attaching statistics / interpolation / combinatorics
/// helpers to Lua's existing `math` global.
pub struct MathModule;

impl LuamlStdlibModule for MathModule {
    fn namespace(&self) -> &'static str {
        "math"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        // Fetch the PRE-EXISTING math table. Lua installs it as part of the
        // standard libraries; we must merge into it rather than replace it.
        let math: Table = lua.globals().get("math")?;

        math.set(
            "mean",
            lua.create_function(|_, list: Vec<f64>| -> mlua::Result<f64> {
                if list.is_empty() {
                    return Err(mlua::Error::runtime("empty list"));
                }
                let sum: f64 = list.iter().sum();
                Ok(sum / list.len() as f64)
            })?,
        )?;

        math.set(
            "median",
            lua.create_function(|_, list: Vec<f64>| -> mlua::Result<f64> {
                if list.is_empty() {
                    return Err(mlua::Error::runtime("empty list"));
                }
                let mut xs = list;
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let n = xs.len();
                let mid = n / 2;
                let value = if n % 2 == 0 {
                    (xs[mid - 1] + xs[mid]) / 2.0
                } else {
                    xs[mid]
                };
                Ok(value)
            })?,
        )?;

        math.set(
            "variance",
            lua.create_function(|_, list: Vec<f64>| -> mlua::Result<f64> {
                if list.is_empty() {
                    return Err(mlua::Error::runtime("empty list"));
                }
                let n = list.len() as f64;
                let mean = list.iter().sum::<f64>() / n;
                let var = list.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
                Ok(var)
            })?,
        )?;

        math.set(
            "stddev",
            lua.create_function(|_, list: Vec<f64>| -> mlua::Result<f64> {
                if list.is_empty() {
                    return Err(mlua::Error::runtime("empty list"));
                }
                let n = list.len() as f64;
                let mean = list.iter().sum::<f64>() / n;
                let var = list.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
                Ok(var.sqrt())
            })?,
        )?;

        math.set(
            "percentile",
            lua.create_function(|_, (list, p): (Vec<f64>, f64)| -> mlua::Result<f64> {
                if list.is_empty() {
                    return Err(mlua::Error::runtime("empty list"));
                }
                if !(0.0..=1.0).contains(&p) || p.is_nan() {
                    return Err(mlua::Error::runtime(
                        "percentile p must be in [0.0, 1.0]",
                    ));
                }
                let mut xs = list;
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let n = xs.len();
                if n == 1 {
                    return Ok(xs[0]);
                }
                // Linear interpolation between nearest ranks:
                // rank position in 0..n-1 domain.
                let rank = p * (n - 1) as f64;
                let lo = rank.floor() as usize;
                let hi = rank.ceil() as usize;
                if lo == hi {
                    return Ok(xs[lo]);
                }
                let frac = rank - lo as f64;
                Ok(xs[lo] + (xs[hi] - xs[lo]) * frac)
            })?,
        )?;

        math.set(
            "lerp",
            lua.create_function(|_, (a, b, t): (f64, f64, f64)| -> mlua::Result<f64> {
                Ok(a + (b - a) * t)
            })?,
        )?;

        math.set(
            "smoothstep",
            lua.create_function(
                |_, (edge0, edge1, x): (f64, f64, f64)| -> mlua::Result<f64> {
                    if edge0 == edge1 {
                        return Err(mlua::Error::runtime("smoothstep edges must differ"));
                    }
                    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
                    Ok(t * t * (3.0 - 2.0 * t))
                },
            )?,
        )?;

        math.set(
            "clamp",
            lua.create_function(
                |_, (x, lo, hi): (f64, f64, f64)| -> mlua::Result<f64> {
                    if lo > hi {
                        return Err(mlua::Error::runtime("clamp lo must be <= hi"));
                    }
                    Ok(x.clamp(lo, hi))
                },
            )?,
        )?;

        // Combinatorics. Judgment call: we return f64 to stay consistent with
        // the rest of this module and to sidestep i64 overflow at moderate n
        // (13! already exceeds u32, 21! exceeds u64). f64 loses exact
        // integrality past 2^53 but gives graceful degradation to infinity
        // rather than a panic / wrap. Callers doing exact integer work on
        // small n can still round the result.
        math.set(
            "factorial",
            lua.create_function(|_, n: i64| -> mlua::Result<f64> {
                if n < 0 {
                    return Err(mlua::Error::runtime("factorial requires n >= 0"));
                }
                let mut acc: f64 = 1.0;
                for i in 2..=n {
                    acc *= i as f64;
                }
                Ok(acc)
            })?,
        )?;

        math.set(
            "binom",
            lua.create_function(|_, (n, k): (i64, i64)| -> mlua::Result<f64> {
                if n < 0 || k < 0 {
                    return Err(mlua::Error::runtime("binom requires n >= 0 and k >= 0"));
                }
                if k > n {
                    return Ok(0.0);
                }
                // Multiplicative formula, iterating over the smaller of k and
                // n-k to keep intermediate products small. Accumulates in f64
                // to match factorial's overflow behavior.
                let k = k.min(n - k);
                let mut acc: f64 = 1.0;
                for i in 0..k {
                    acc = acc * (n - i) as f64 / (i + 1) as f64;
                }
                Ok(acc)
            })?,
        )?;

        Ok(math)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install_math(lua: &Lua) {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let table = MathModule
            .install(lua, &rt.handle().clone())
            .expect("install math");
        lua.globals().set("math", table).unwrap();
    }

    #[test]
    fn mean_of_1_2_3_is_2() {
        let lua = Lua::new();
        install_math(&lua);
        let v: f64 = lua.load("return math.mean({1, 2, 3})").eval().unwrap();
        assert!((v - 2.0).abs() < 1e-12);
    }

    #[test]
    fn median_of_even_length_is_average_of_middle_pair() {
        let lua = Lua::new();
        install_math(&lua);
        let v: f64 = lua
            .load("return math.median({1, 2, 3, 4})")
            .eval()
            .unwrap();
        assert!((v - 2.5).abs() < 1e-12);
    }

    #[test]
    fn lerp_half_is_midpoint() {
        let lua = Lua::new();
        install_math(&lua);
        let v: f64 = lua.load("return math.lerp(0, 10, 0.5)").eval().unwrap();
        assert!((v - 5.0).abs() < 1e-12);
    }

    #[test]
    fn clamp_respects_bounds() {
        let lua = Lua::new();
        install_math(&lua);
        let lo: f64 = lua.load("return math.clamp(-5, 0, 10)").eval().unwrap();
        let hi: f64 = lua.load("return math.clamp(99, 0, 10)").eval().unwrap();
        let mid: f64 = lua.load("return math.clamp(7, 0, 10)").eval().unwrap();
        assert_eq!(lo, 0.0);
        assert_eq!(hi, 10.0);
        assert_eq!(mid, 7.0);
    }

    #[test]
    fn factorial_of_5_is_120() {
        let lua = Lua::new();
        install_math(&lua);
        let v: f64 = lua.load("return math.factorial(5)").eval().unwrap();
        assert_eq!(v, 120.0);
    }

    #[test]
    fn binom_5_2_is_10() {
        let lua = Lua::new();
        install_math(&lua);
        let v: f64 = lua.load("return math.binom(5, 2)").eval().unwrap();
        assert_eq!(v, 10.0);
    }

    #[test]
    fn builtin_math_pi_survives_installation() {
        let lua = Lua::new();
        install_math(&lua);
        let pi: f64 = lua.load("return math.pi").eval().unwrap();
        assert!((pi - std::f64::consts::PI).abs() < 1e-12);
        // And a built-in function like math.sqrt should still work.
        let sqrt: f64 = lua.load("return math.sqrt(9)").eval().unwrap();
        assert_eq!(sqrt, 3.0);
    }

    #[test]
    fn percentile_linear_interpolation() {
        let lua = Lua::new();
        install_math(&lua);
        // p=0.0 -> first element, p=1.0 -> last element
        let lo: f64 = lua
            .load("return math.percentile({10, 20, 30, 40}, 0.0)")
            .eval()
            .unwrap();
        let hi: f64 = lua
            .load("return math.percentile({10, 20, 30, 40}, 1.0)")
            .eval()
            .unwrap();
        assert_eq!(lo, 10.0);
        assert_eq!(hi, 40.0);
        // p=0.5 over 4 points at ranks 0,1,2,3 -> rank 1.5 -> halfway between 20 and 30
        let mid: f64 = lua
            .load("return math.percentile({10, 20, 30, 40}, 0.5)")
            .eval()
            .unwrap();
        assert!((mid - 25.0).abs() < 1e-12);
    }

    #[test]
    fn mean_of_empty_list_errors() {
        let lua = Lua::new();
        install_math(&lua);
        let res = lua.load("return math.mean({})").eval::<f64>();
        assert!(res.is_err());
    }
}

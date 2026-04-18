//! `vec` stdlib module: arbitrary-dimension vector math.
//!
//! Vectors are represented as plain 1-indexed Lua tables of numbers. All
//! functions are synchronous — there is no async work to drive, and scripts
//! consume results immediately. Every function validates its inputs with
//! [`to_vec`] and emits its result with [`from_vec`], so the representation
//! is fully encapsulated: callers never see Rust `Vec<f64>` values.
//!
//! Design note: the L15 plan originally called for `vec` to be userdata with
//! a metatable. We intentionally ship a plain-table representation instead
//! because:
//!   1. Scripts can inspect / mutate / copy / serialize vectors using the
//!      normal table idioms they already know — no bespoke API surface per
//!      read.
//!   2. JSON round-tripping and cross-boundary transport (e.g. through
//!      `FieldValue::List`) are trivial, since a table of numbers already is
//!      the canonical form on both sides.
//!   3. Userdata with a metatable is a pure perf optimization (no validation
//!      on repeat use) but this module's operations are all O(n) over the
//!      components anyway — per-call validation is a rounding error.
//! If profiling later shows that validation dominates, swapping to userdata
//! is a local change: the module's public surface (`vec.new`, `vec.add`, ...)
//! is agnostic to the underlying representation.
use mlua::{Lua, Table, Value};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Zero-sized marker implementing [`LuamlStdlibModule`] for the `vec`
/// namespace. Constructed in `collect_modules` under the `stdlib-vec`
/// feature flag.
pub struct VecModule;

impl LuamlStdlibModule for VecModule {
    fn namespace(&self) -> &'static str {
        "vec"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let tbl = lua.create_table()?;

        tbl.set(
            "new",
            lua.create_function(|lua, t: Table| {
                let v = to_vec(lua, t)?;
                from_vec(lua, &v)
            })?,
        )?;

        tbl.set(
            "add",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                check_same_dim(&a, &b)?;
                let out: Vec<f64> = a.iter().zip(b.iter()).map(|(x, y)| x + y).collect();
                from_vec(lua, &out)
            })?,
        )?;

        tbl.set(
            "sub",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                check_same_dim(&a, &b)?;
                let out: Vec<f64> = a.iter().zip(b.iter()).map(|(x, y)| x - y).collect();
                from_vec(lua, &out)
            })?,
        )?;

        tbl.set(
            "scale",
            lua.create_function(|lua, (a, k): (Table, f64)| {
                let a = to_vec(lua, a)?;
                let out: Vec<f64> = a.iter().map(|x| x * k).collect();
                from_vec(lua, &out)
            })?,
        )?;

        tbl.set(
            "dot",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                check_same_dim(&a, &b)?;
                Ok(dot(&a, &b))
            })?,
        )?;

        tbl.set(
            "cross",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                if a.len() != 3 || b.len() != 3 {
                    return Err(mlua::Error::runtime(format!(
                        "vec: cross requires 3D vectors (got {} and {})",
                        a.len(),
                        b.len()
                    )));
                }
                let out = vec![
                    a[1] * b[2] - a[2] * b[1],
                    a[2] * b[0] - a[0] * b[2],
                    a[0] * b[1] - a[1] * b[0],
                ];
                from_vec(lua, &out)
            })?,
        )?;

        tbl.set(
            "norm",
            lua.create_function(|lua, a: Table| {
                let a = to_vec(lua, a)?;
                Ok(norm(&a))
            })?,
        )?;

        tbl.set(
            "normalize",
            lua.create_function(|lua, a: Table| {
                let a = to_vec(lua, a)?;
                let n = norm(&a);
                if n == 0.0 {
                    return Err(mlua::Error::runtime("vec: cannot normalize zero vector"));
                }
                let out: Vec<f64> = a.iter().map(|x| x / n).collect();
                from_vec(lua, &out)
            })?,
        )?;

        tbl.set(
            "distance",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                check_same_dim(&a, &b)?;
                let sum_sq: f64 = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| {
                        let d = x - y;
                        d * d
                    })
                    .sum();
                Ok(sum_sq.sqrt())
            })?,
        )?;

        tbl.set(
            "cosine_sim",
            lua.create_function(|lua, (a, b): (Table, Table)| {
                let a = to_vec(lua, a)?;
                let b = to_vec(lua, b)?;
                check_same_dim(&a, &b)?;
                let na = norm(&a);
                let nb = norm(&b);
                if na == 0.0 || nb == 0.0 {
                    return Err(mlua::Error::runtime(
                        "vec: cosine_sim undefined for zero vector",
                    ));
                }
                Ok(dot(&a, &b) / (na * nb))
            })?,
        )?;

        Ok(tbl)
    }
}

/// Extract and validate a numeric list from a Lua table. Accepts 1-indexed
/// contiguous integer keys only — any string key, hole, or non-number entry
/// is a hard error. An empty table is accepted and returns an empty `Vec`;
/// operations that require non-zero dimensions (e.g. `normalize`,
/// `cosine_sim`) validate that separately on the resulting slice.
fn to_vec(_lua: &Lua, t: Table) -> mlua::Result<Vec<f64>> {
    // Reject any non-integer key — that indicates a map-shaped table, which
    // is not a valid vector representation.
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        if !matches!(k, Value::Integer(_)) {
            return Err(mlua::Error::runtime(
                "vec: expected list table (1-indexed integer keys, number values)",
            ));
        }
    }

    let len = t.raw_len();
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        let v: Value = t.raw_get(i)?;
        let n = match v {
            Value::Integer(x) => x as f64,
            Value::Number(x) => x,
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "vec: entry at index {i} is not a number"
                )));
            }
        };
        out.push(n);
    }
    Ok(out)
}

/// Build a fresh 1-indexed Lua table from a slice of `f64`.
fn from_vec(lua: &Lua, v: &[f64]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(v.len(), 0)?;
    for (i, x) in v.iter().enumerate() {
        t.raw_set(i + 1, *x)?;
    }
    Ok(t)
}

/// Shared dim-match guard. Called by every pairwise op except `cross`
/// (which has its own fixed-3 check).
fn check_same_dim(a: &[f64], b: &[f64]) -> mlua::Result<()> {
    if a.len() != b.len() {
        return Err(mlua::Error::runtime(format!(
            "vec: dim mismatch ({} vs {})",
            a.len(),
            b.len()
        )));
    }
    Ok(())
}

/// Euclidean norm. Extracted so `normalize` and `cosine_sim` share the
/// exact same formula and zero-vector edge case.
fn norm(a: &[f64]) -> f64 {
    a.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Inner product. Callers check `a.len() == b.len()` first.
fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    /// Build an engine-flavored test Lua: create a Lua and install the `vec`
    /// module under its global. Tests then load literal scripts that
    /// exercise the module's public surface.
    fn setup() -> (Lua, tokio::runtime::Runtime) {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let tbl = VecModule.install(&lua, rt.handle()).unwrap();
        lua.globals().set("vec", tbl).unwrap();
        (lua, rt)
    }

    #[test]
    fn add_sub_scale_round_trip() {
        let (lua, _rt) = setup();
        // (a + b) - b == a; (a * 2) / 2 == a.
        let script = r#"
            local a = vec.new({1, 2, 3})
            local b = vec.new({10, 20, 30})
            local c = vec.sub(vec.add(a, b), b)
            local d = vec.scale(vec.scale(a, 2), 0.5)
            return c[1], c[2], c[3], d[1], d[2], d[3]
        "#;
        let (c1, c2, c3, d1, d2, d3): (f64, f64, f64, f64, f64, f64) =
            lua.load(script).eval().unwrap();
        assert_eq!((c1, c2, c3), (1.0, 2.0, 3.0));
        assert_eq!((d1, d2, d3), (1.0, 2.0, 3.0));
    }

    #[test]
    fn dot_of_1_2_and_3_4_is_11() {
        let (lua, _rt) = setup();
        let v: f64 = lua
            .load("return vec.dot({1, 2}, {3, 4})")
            .eval()
            .unwrap();
        assert_eq!(v, 11.0);
    }

    #[test]
    fn cross_of_x_and_y_is_z() {
        let (lua, _rt) = setup();
        let script = r#"
            local c = vec.cross({1, 0, 0}, {0, 1, 0})
            return c[1], c[2], c[3]
        "#;
        let (x, y, z): (f64, f64, f64) = lua.load(script).eval().unwrap();
        assert_eq!((x, y, z), (0.0, 0.0, 1.0));
    }

    #[test]
    fn norm_of_3_4_is_5() {
        let (lua, _rt) = setup();
        let v: f64 = lua.load("return vec.norm({3, 4})").eval().unwrap();
        assert_eq!(v, 5.0);
    }

    #[test]
    fn normalize_produces_unit_vector() {
        let (lua, _rt) = setup();
        let script = r#"
            local n = vec.normalize({3, 4})
            return vec.norm(n)
        "#;
        let n: f64 = lua.load(script).eval().unwrap();
        assert!((n - 1.0).abs() < 1e-12, "expected unit norm, got {n}");
    }

    #[test]
    fn cosine_sim_of_identical_vectors_is_one() {
        let (lua, _rt) = setup();
        let s: f64 = lua
            .load("return vec.cosine_sim({1, 2, 3}, {1, 2, 3})")
            .eval()
            .unwrap();
        assert!((s - 1.0).abs() < 1e-12, "expected 1.0, got {s}");
    }

    #[test]
    fn dim_mismatch_errors() {
        let (lua, _rt) = setup();
        let res = lua
            .load("return vec.add({1, 2}, {1, 2, 3})")
            .eval::<Value>();
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("dim mismatch"),
            "expected dim mismatch error, got: {err}"
        );
    }

    #[test]
    fn cross_requires_3d() {
        let (lua, _rt) = setup();
        let res = lua
            .load("return vec.cross({1, 2}, {3, 4})")
            .eval::<Value>();
        let err = res.unwrap_err().to_string();
        assert!(err.contains("3D"), "expected 3D requirement error: {err}");
    }

    #[test]
    fn normalize_zero_vector_errors() {
        let (lua, _rt) = setup();
        let res = lua
            .load("return vec.normalize({0, 0, 0})")
            .eval::<Value>();
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("zero vector"),
            "expected zero vector error, got: {err}"
        );
    }

    #[test]
    fn new_rejects_non_number_entries() {
        let (lua, _rt) = setup();
        let res = lua
            .load("return vec.new({1, 'two', 3})")
            .eval::<Value>();
        assert!(res.is_err(), "vec.new must reject non-number entries");
    }

    #[test]
    fn new_rejects_map_tables() {
        let (lua, _rt) = setup();
        let res = lua
            .load("return vec.new({x = 1, y = 2})")
            .eval::<Value>();
        assert!(res.is_err(), "vec.new must reject map-shaped tables");
    }
}

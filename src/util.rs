//! Small things the rest share: the clock, local times, paths as Python's `pathlib` gave them, and
//! JSON read the way the Python rota read it (a missing, null, false, 0 or "" value is "not given").

use std::ffi::{CStr, CString};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serializer};
use serde_json::Value;

/// Seconds since the epoch, as `time.time()`.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// An environment variable, unless it's unset or empty.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `~/…` made absolute, as `Path.expanduser()`.
pub fn expand(path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match path.strip_prefix('~') {
        Some("") => home().unwrap_or_else(|| PathBuf::from(path)),
        Some(rest) if rest.starts_with('/') => match home() {
            Some(h) => h.join(&rest[1..]),
            None => PathBuf::from(path),
        },
        _ => PathBuf::from(path),
    }
}

/// A path made absolute with its symlinks resolved as far as it exists, as `Path.resolve()`.
pub fn resolve(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    };
    if let Ok(real) = std::fs::canonicalize(&abs) {
        return real;
    }
    let mut out = PathBuf::from("/");
    let mut exists = true;
    for part in abs.components() {
        match part {
            Component::Normal(name) => {
                out.push(name);
                if exists {
                    match std::fs::canonicalize(&out) {
                        Ok(real) => out = real,
                        Err(_) => exists = false,
                    }
                }
            }
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// A program on $PATH, as `shutil.which`.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// An epoch time in this machine's own timezone, as `time.strftime(fmt, time.localtime(t))`.
pub fn local_time(t: f64, fmt: &str) -> String {
    let secs = t.floor() as _; // a time_t, whatever its width here
    let fmt = CString::new(fmt).unwrap_or_default();
    let mut buf = [0u8; 64];
    // SAFETY: localtime_r fills `tm`, and strftime writes at most buf.len() bytes, NUL included
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::new();
        }
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm);
        if n == 0 {
            return String::new();
        }
        CStr::from_ptr(buf.as_ptr().cast()).to_string_lossy().into_owned()
    }
}

/// Whether a JSON value is "given", as Python's truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// A JSON value as text, "" when it isn't given: `str(p.get(key) or "")`.
pub fn text(v: Option<&Value>) -> String {
    match v {
        _ if !truthy(v) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(_)) => "True".into(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// A number as Python's `round(x, 1)`.
pub fn round1(x: f64) -> f64 {
    (x * 10.0).round_ties_even() / 10.0
}

/// An epoch time written as JSON the way it came: whole seconds as an integer, as Claude and Codex give it.
pub fn epoch<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(x) if x.fract() == 0.0 && x.abs() < 9.0e15 => s.serialize_i64(*x as i64),
        Some(x) => s.serialize_f64(*x),
        None => s.serialize_none(),
    }
}

/// A field that's `null` in the file read as its default, as Python's `d.get(key) or {}`.
pub fn or_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

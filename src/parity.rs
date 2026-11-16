//! Rules shared with the Python Road Viewer API: error bodies, text validation, recording order and
//! download names. Python's ingress errors carry a Korean sentence in `error`; HARS keeps a stable
//! machine code there and puts Python's sentence in `message`, which the reused UI shows first.
use crate::error::ApiError;
use axum::{
    Json,
    http::{StatusCode, header as h},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{cmp::Ordering, sync::LazyLock};
use unicode_general_category::{GeneralCategory, get_general_category};

pub const NOT_FOUND: &str = "로그 또는 파일을 찾을 수 없습니다.";
pub const TOO_LARGE: &str = "업로드 용량 제한을 초과했습니다.";
pub const STORED_KINDS: [&str; 7] = [
    "rlog.zst",
    "qcamera.ts",
    "fcamera.hevc",
    "ecamera.hevc",
    "camera.mp4",
    "fcamera.mp4",
    "ecamera.mp4",
];
/// (source, original, kept MP4) in Python's `video_sources.SOURCES` order.
pub const SOURCES: [(&str, &str, &str); 3] = [
    ("qcamera", "qcamera.ts", "camera.mp4"),
    ("front", "fcamera.hevc", "fcamera.mp4"),
    ("wide", "ecamera.hevc", "ecamera.mp4"),
];
pub const TAG_COLORS: [&str; 12] = [
    "mint", "blue", "violet", "amber", "rose", "slate", "red", "coral", "lime", "yellow", "cyan",
    "indigo",
];

#[derive(Debug)]
pub struct Fail {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: Option<String>,
}
pub type Answer<T> = std::result::Result<T, Fail>;
impl Fail {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: Some(message.into()),
        }
    }
    pub fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", NOT_FOUND)
    }
    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            TOO_LARGE,
        )
    }
}
impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        if e.0 == StatusCode::NOT_FOUND && e.1 == "not_found" {
            return Self::not_found();
        }
        Self {
            status: e.0,
            code: e.1,
            message: None,
        }
    }
}
impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        ApiError::from(e).into()
    }
}
impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let body = match self.message {
            Some(message) => json!({"error":self.code,"message":message}),
            None => json!({"error":self.code}),
        };
        (self.status, Json(body)).into_response()
    }
}

/// Python `str.isspace`: Unicode White_Space plus the ASCII information separators.
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}
/// `' '.join(s.split())`.
pub fn py_words(s: &str) -> String {
    s.split(py_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
/// `unicodedata.category(c).startswith('C')`.
pub fn other(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
    )
}
pub fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}
/// Python's `isinstance(v,(int,float))` for JSON (bool is a separate JSON type).
pub fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|_| v.is_number())
}

/// Big hexadecimal/decimal numbers compared as Python compares `int`s.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Digits(String);
impl Digits {
    fn new(s: &str) -> Self {
        let t = s.trim_start_matches('0').to_ascii_lowercase();
        Self(if t.is_empty() { "0".into() } else { t })
    }
}
impl Ord for Digits {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}
impl PartialOrd for Digits {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `server.recording_order`: numbered routes after custom names, each route kept together.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingOrder {
    Custom {
        uploaded: f64,
        id: String,
    },
    Numbered {
        dongle: String,
        route: String,
        segment: String,
        id: String,
    },
}
static ORDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^([0-9a-fA-F]+)--([0-9a-fA-F]+)(?:\s*/\s*구간\s*|--)([0-9]+)$").unwrap()
});
impl RecordingOrder {
    pub fn of(meta: &Value) -> Self {
        let id = meta["id"].as_str().unwrap_or("").to_owned();
        if let Some(c) = ORDER.captures(meta["name"].as_str().unwrap_or("")) {
            return Self::Numbered {
                dongle: Digits::new(&c[1]).0,
                route: c[2].to_lowercase(),
                segment: Digits::new(&c[3]).0,
                id,
            };
        }
        Self::Custom {
            uploaded: number(&meta["uploaded"]).unwrap_or(0.),
            id,
        }
    }
    /// The drive's key (`'%x--%s'`), or None for a custom name.
    pub fn route_key(&self) -> Option<String> {
        match self {
            Self::Numbered { dongle, route, .. } => Some(format!("{dongle}--{route}")),
            Self::Custom { .. } => None,
        }
    }
    pub fn segment(&self) -> Option<u64> {
        match self {
            Self::Numbered { segment, .. } => segment.parse().ok(),
            Self::Custom { .. } => None,
        }
    }
}
impl Eq for RecordingOrder {}
impl Ord for RecordingOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Custom { .. }, Self::Numbered { .. }) => Ordering::Less,
            (Self::Numbered { .. }, Self::Custom { .. }) => Ordering::Greater,
            (
                Self::Custom { uploaded: a, id: x },
                Self::Custom {
                    uploaded: b, id: y, ..
                },
            ) => a.total_cmp(b).then_with(|| x.cmp(y)),
            (
                Self::Numbered {
                    dongle: d1,
                    route: r1,
                    segment: s1,
                    id: i1,
                },
                Self::Numbered {
                    dongle: d2,
                    route: r2,
                    segment: s2,
                    id: i2,
                },
            ) => Digits(d1.clone())
                .cmp(&Digits(d2.clone()))
                .then_with(|| r1.cmp(r2))
                .then_with(|| Digits(s1.clone()).cmp(&Digits(s2.clone())))
                .then_with(|| i1.cmp(i2)),
        }
    }
}
impl PartialOrd for RecordingOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `server.download_name`: the uploaded filename; a kept MP4 takes its original's name.
pub fn download_name(meta: &Value, kind: &str) -> String {
    let source = SOURCES
        .iter()
        .find(|(_, _, mp4)| *mp4 == kind)
        .map(|(_, raw, _)| *raw)
        .unwrap_or(kind);
    let files = &meta["files"];
    let base = |v: &str| v.rsplit('/').next().unwrap_or(v).to_owned();
    let mut name = files[source]
        .as_str()
        .or_else(|| files[kind].as_str())
        .map(base)
        .unwrap_or_else(|| source.to_owned());
    let log = files["rlog.zst"].as_str().map(base).unwrap_or_default();
    if files.get(source).is_none() && files.get(kind).is_none() && log.ends_with("--rlog.zst") {
        name = format!("{}{source}", &log[..log.len() - "rlog.zst".len()]);
    }
    if kind.ends_with(".mp4") {
        // PurePosixPath.with_suffix: a leading dot is part of the stem.
        let stem = match name.rfind('.') {
            Some(i) if i > 0 => &name[..i],
            _ => &name,
        };
        format!("{stem}.mp4")
    } else {
        name
    }
}

fn rfc5987(name: &str, safe: &[u8]) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || safe.contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
/// Werkzeug `send_file(as_attachment=True, download_name=...)`.
pub fn send_file_disposition(name: &str) -> String {
    if name.is_ascii() {
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if token {
            format!("attachment; filename={name}")
        } else {
            format!(
                "attachment; filename=\"{}\"",
                name.replace('\\', "\\\\").replace('"', "\\\"")
            )
        }
    } else {
        use unicode_normalization::UnicodeNormalization;
        let simple = name.nfkd().filter(char::is_ascii).collect::<String>();
        format!(
            "{}; filename*=UTF-8''{}",
            send_file_disposition(&simple),
            rfc5987(name, b"!#$&+-.^_`|~")
        )
    }
}
/// `email.message.Message.add_header('Content-Disposition','attachment',filename=name)`.
pub fn message_disposition(name: &str) -> String {
    if name.is_ascii() {
        format!(
            "attachment; filename=\"{}\"",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        format!("attachment; filename*=utf-8''{}", rfc5987(name, b"_.-~"))
    }
}

/// Local calendar time (Python `time.localtime`) as (year, month, day, hour, minute, second).
pub fn local_time(seconds: f64) -> (i32, u32, u32, u32, u32, u32) {
    #[cfg(unix)]
    {
        let t = seconds.floor() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r writes only into the provided struct.
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return (
                tm.tm_year + 1900,
                tm.tm_mon as u32 + 1,
                tm.tm_mday as u32,
                tm.tm_hour as u32,
                tm.tm_min as u32,
                tm.tm_sec as u32,
            );
        }
    }
    let days = (seconds / 86400.).floor() as i64;
    let rest = (seconds - days as f64 * 86400.) as u32;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day, rest / 3600, rest / 60 % 60, rest % 60)
}

/// Unknown paths answer 404 as Python does. Every Python route is registered, so nothing answers 501.
pub async fn fallback() -> Response {
    Fail::not_found().into_response()
}
pub async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"error":"method_not_allowed"})),
    )
        .into_response()
}
/// Axum's own 413 (body limit) is plain text; Python answers JSON.
pub fn json_too_large(device: bool, mut response: Response) -> Response {
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE
        && !response
            .headers()
            .get(h::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"application/json"))
    {
        let mut replaced = if device {
            ApiError(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large").into_response()
        } else {
            Fail::too_large().into_response()
        };
        std::mem::swap(response.headers_mut(), replaced.headers_mut());
        response
            .headers_mut()
            .insert(h::CONTENT_TYPE, "application/json".parse().unwrap());
        response.headers_mut().remove(h::CONTENT_LENGTH);
        *response.body_mut() = std::mem::take(replaced.body_mut());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_matches_python_int_tuples() {
        let a = RecordingOrder::of(&json!({"id":"a","name":"000003fe--d524dc1e0b / 구간 9"}));
        let b = RecordingOrder::of(&json!({"id":"b","name":"3FE--D524DC1E0B--10"}));
        let c = RecordingOrder::of(&json!({"id":"c","name":"custom","uploaded":5}));
        assert!(c < a && a < b);
        assert_eq!(a.route_key().unwrap(), "3fe--d524dc1e0b");
        assert_eq!(b.route_key(), a.route_key());
        assert_eq!(b.segment(), Some(10));
    }
    #[test]
    fn names_and_dispositions_follow_python() {
        let m = json!({"files":{"rlog.zst":"dir/000003fe--d524dc1e0b--8--rlog.zst"}});
        assert_eq!(
            download_name(&m, "fcamera.mp4"),
            "000003fe--d524dc1e0b--8--fcamera.mp4"
        );
        assert_eq!(download_name(&json!({}), "camera.mp4"), "qcamera.mp4");
        assert_eq!(
            send_file_disposition("a--1--rlog.zst"),
            "attachment; filename=a--1--rlog.zst"
        );
        assert_eq!(
            send_file_disposition("한 a.ts"),
            "attachment; filename=\" a.ts\"; filename*=UTF-8''%ED%95%9C%20a.ts"
        );
        assert_eq!(
            message_disposition("x.tar"),
            "attachment; filename=\"x.tar\""
        );
        assert!(other('\u{200d}') && other('\u{e000}') && !other('한') && py_space('\x1f'));
        assert_eq!(py_words(" a \n\x1c b "), "a b");
    }
}

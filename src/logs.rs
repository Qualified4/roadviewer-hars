//! Bounded streaming Cap'n Proto reader. Replay projection is tracked separately.
use crate::log_capnp;
use anyhow::{Context, bail};
use capnp::{dynamic_value::Reader as Dynamic, serialize};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufReader, Read},
    path::Path,
    sync::LazyLock,
};

pub const LOG_LIMIT: u64 = 512 * 1024 * 1024;
static SPECS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../resources/log-fields.json"))
        .expect("valid compiled field selection")
});
#[derive(Debug, Serialize, Deserialize)]
pub struct Sample {
    pub stamp: u64,
    pub valid: bool,
    pub value: Value,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Decoded {
    pub counts: BTreeMap<String, u64>,
    pub streams: BTreeMap<String, Vec<Sample>>,
    pub bytes: u64,
    #[serde(default)]
    pub starts: crate::track::Starts,
}

struct Limited<R> {
    inner: R,
    left: u64,
    read: u64,
}
impl<R: Read> Read for Limited<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let _profile = crate::profile::Scope::new("zstd_read");
        if out.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut byte = [0];
            return match self.inner.read(&mut byte)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other("decompressed log exceeds 512 MiB")),
            };
        }
        let max = out.len().min(self.left as usize);
        let n = self.inner.read(&mut out[..max])?;
        self.left -= n as u64;
        self.read += n as u64;
        Ok(n)
    }
}

pub fn decode(path: &Path) -> anyhow::Result<Decoded> {
    let _profile = crate::profile::Scope::new("log_decode");
    let source = std::fs::File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(source)?;
    decoder.window_log_max(29)?;
    let mut input = Limited {
        inner: decoder,
        left: LOG_LIMIT,
        read: 0,
    };
    let mut result = Decoded::default();
    // Stream one framed message at a time; never materialize the decompressed 512 MiB log.
    let opts = capnp::message::ReaderOptions {
        traversal_limit_in_words: Some(8 * 1024 * 1024),
        nesting_limit: 64,
    };
    while let Some(message) = serialize::try_read_message(&mut input, opts)? {
        let event = message.get_root::<log_capnp::event::Reader>()?;
        let dynamic = capnp::dynamic_value::Reader::from(event);
        let Dynamic::Struct(reader) = dynamic else {
            bail!("invalid event root");
        };
        let Some(field) = reader.which()? else {
            *result.counts.entry("unknownEvent".into()).or_default() += 1;
            continue;
        };
        let kind = field.get_proto().get_name()?.to_str()?.to_owned();
        *result.counts.entry(kind.clone()).or_default() += 1;
        result.starts.note(&kind, event.get_log_mono_time());
        if matches!(
            kind.as_str(),
            "modelV2"
                | "carState"
                | "carControl"
                | "carOutput"
                | "controlsState"
                | "selfdriveState"
                | "liveParameters"
                | "carParams"
                | "liveCalibration"
                | "radarState"
                | "liveTracks"
                | "qRoadEncodeIdx"
                | "roadEncodeIdx"
                | "wideRoadEncodeIdx"
                | "sendcan"
                | "longitudinalPlan"
                | "gpsLocationExternal"
                | "gpsLocation"
                | "carrotMan"
                | "clocks"
                | "initData"
                | "deviceState"
                | "roadCameraState"
                | "wideRoadCameraState"
        ) {
            let mut value =
                to_json(reader.get(field)?, SPECS.get(&kind), 0).context("decode event fields")?;
            result
                .starts
                .add(&kind, event.get_log_mono_time(), event.get_valid(), &value);
            if kind == "sendcan" {
                if let Some(messages) = value.as_array_mut() {
                    messages.retain(|m| {
                        matches!(m["address"].as_u64(), Some(0x161 | 0x162))
                            && m["src"].as_u64().is_some_and(|s| s < 128)
                            && m["dat"].as_str().is_some_and(|s| s.len() == 64)
                    });
                }
                if value.as_array().is_none_or(|v| v.is_empty()) {
                    continue;
                }
            }
            result.streams.entry(kind).or_default().push(Sample {
                stamp: event.get_log_mono_time(),
                valid: event.get_valid(),
                value,
            });
        }
    }
    result.bytes = input.read;
    Ok(result)
}

fn to_json(value: Dynamic<'_>, selection: Option<&Value>, depth: usize) -> anyhow::Result<Value> {
    if depth > 48 {
        bail!("event nesting limit");
    }
    Ok(match value {
        Dynamic::Void => Value::Null,
        Dynamic::Bool(v) => json!(v),
        Dynamic::Int8(v) => json!(v),
        Dynamic::Int16(v) => json!(v),
        Dynamic::Int32(v) => json!(v),
        Dynamic::Int64(v) => json!(v),
        Dynamic::UInt8(v) => json!(v),
        Dynamic::UInt16(v) => json!(v),
        Dynamic::UInt32(v) => json!(v),
        Dynamic::UInt64(v) => json!(v),
        Dynamic::Float32(v) => json!(v),
        Dynamic::Float64(v) => json!(v),
        Dynamic::Text(v) => json!(v.to_str()?),
        Dynamic::Data(v) => json!(hex::encode(v)),
        Dynamic::Enum(v) => match v.get_enumerant()? {
            Some(e) => json!(e.get_proto().get_name()?.to_str()?),
            None => json!(v.get_value()),
        },
        Dynamic::List(v) => {
            let mut out = Vec::with_capacity(v.len() as usize);
            for i in 0..v.len() {
                out.push(to_json(v.get(i)?, selection, depth + 1)?);
            }
            Value::Array(out)
        }
        Dynamic::Struct(v) => {
            let mut out = serde_json::Map::new();
            let active = v.which()?;
            for field in v.get_schema().get_fields()? {
                let proto = field.get_proto();
                let name = proto.get_name()?.to_str()?;
                if selection
                    .and_then(Value::as_object)
                    .is_some_and(|s| !s.contains_key(name))
                {
                    continue;
                }
                let discr = proto.get_discriminant_value();
                if discr != u16::MAX
                    && active.is_none_or(|a| a.get_proto().get_discriminant_value() != discr)
                {
                    continue;
                }
                if matches!(
                    field.get_type().which(),
                    capnp::introspect::TypeVariant::Text
                        | capnp::introspect::TypeVariant::Data
                        | capnp::introspect::TypeVariant::Struct(_)
                        | capnp::introspect::TypeVariant::List(_)
                        | capnp::introspect::TypeVariant::AnyPointer
                        | capnp::introspect::TypeVariant::Capability
                ) && !v.has(field)?
                {
                    continue;
                }
                out.insert(
                    proto.get_name()?.to_str()?.to_owned(),
                    to_json(
                        v.get(field)?,
                        selection.and_then(|s| s.get(name)),
                        depth + 1,
                    )?,
                );
            }
            Value::Object(out)
        }
        Dynamic::AnyPointer(_) | Dynamic::Capability(_) => Value::Null,
    })
}

pub fn write_gzip(path: &Path, value: &Value) -> anyhow::Result<()> {
    let _profile = crate::profile::Scope::new("json_gzip_write");
    use std::io::Write;
    let file = std::fs::File::create(path)?;
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
    // Feed the compressor blocks rather than a call per JSON punctuation/number.
    // This bounds extra memory while keeping streaming and compression level 9.
    {
        let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, &mut gz);
        serde_json::to_writer(&mut buffered, value)?;
        buffered.into_inner().map_err(|e| e.into_error())?;
    }
    gz.flush()?;
    gz.finish()?.sync_all()?;
    Ok(())
}
pub fn read_gzip(path: &Path) -> anyhow::Result<Value> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(BufReader::new(file))
        .take(LOG_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LOG_LIMIT {
        bail!("prepared data exceeds limit");
    }
    Ok(serde_json::from_slice(&bytes)?)
}
